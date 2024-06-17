use anyhow::{ensure, Context, Error, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use futures_util::TryStreamExt;
use page_turner::prelude::*;
use parse_link_header::parse_with_rel;
use reqwest::header::{self, HeaderMap};
use reqwest::Client;
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde::{Deserialize, Serialize};
use serde_json::Number;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(clap::ValueEnum, Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Platform {
    /// Meris LVX
    #[default]
    MerisLvx,
    /// ZOIA
    Zoia,
}

#[derive(Debug, Deserialize)]
struct Patch {
    id: Number,
    slug: String,
}

#[derive(Clone)]
struct GetPatchesRequest {
    platform: usize,
    page: usize,
}

impl GetPatchesRequest {
    fn build(&self) -> String {
        format!(
            "https://patchstorage.com/api/beta/patches/?platforms={}&page={}",
            self.platform, self.page
        )
    }
}

struct PatchesPage {
    patches: Vec<Patch>,
    has_next: bool,
}

struct PagedPatches {
    client: ClientWithMiddleware,
}

impl PagedPatches {
    async fn get_patches_page(&self, request: GetPatchesRequest) -> Result<PatchesPage> {
        let response = self.client.get(&request.build()).send().await?;
        let has_next = self.has_next(&response.headers())?;
        let patches = response.json::<Vec<Patch>>().await?;
        Ok(PatchesPage { patches, has_next })
    }

    fn has_next(&self, headers: &HeaderMap) -> Result<bool> {
        let link_header = headers
            .get(header::LINK)
            .context("missing Link header")?
            .to_str()?;
        let rel_map = parse_with_rel(link_header)?;
        Ok(rel_map.get("next").is_some())
    }
}

impl PageTurner<GetPatchesRequest> for PagedPatches {
    type PageItems = Vec<Patch>;
    type PageError = Error;

    async fn turn_page(
        &self,
        mut request: GetPatchesRequest,
    ) -> TurnedPageResult<Self, GetPatchesRequest> {
        let response = self.get_patches_page(request.clone()).await?;
        if response.has_next {
            request.page += 1;
            Ok(TurnedPage::next(response.patches, request))
        } else {
            Ok(TurnedPage::last(response.patches))
        }
    }
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PatchMetaData {
    id: Number,
    url: String,
    slug: String,
    title: String,
    content: String,
    files: Vec<PatchFile>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct PatchFile {
    id: Number,
    url: String,
    filesize: Number,
    filename: String,
}

async fn get_patch_metadata(client: &ClientWithMiddleware, id: u64) -> Result<PatchMetaData> {
    let url = format!("https://patchstorage.com/api/beta/patches/{id}");
    let response = client.get(&url).send().await?;
    let metadata = response.json::<PatchMetaData>().await?;
    Ok(metadata)
}

async fn get_patch_bytes(client: &ClientWithMiddleware, url: &str) -> Result<Vec<u8>> {
    let response = client.get(url).send().await?;
    let bytes = response.bytes().await?;
    Ok(bytes.to_vec())
}

fn sysex_filter(buf: &[u8]) -> Option<&[u8]> {
    let len = buf.len();
    let mut first = len;
    for i in 0..len {
        if buf[i] >= 0xF0 {
            first = i;
            break;
        }
    }
    if first == len || buf[first] != 0xF0 {
        // F0 not found
        // TODO: or found another system message
        return None;
    }
    let mut last = len;
    for j in (first + 1)..len {
        if buf[j] == 0xF7 {
            last = j;
            break;
        }
    }
    if last == len {
        // F7 not found
        return None;
    }
    if first > 0 || last < len - 1 {
        return Some(&buf[first..=last]);
    }
    None // Nothing trimmed
}

#[derive(Debug, Parser)]
#[clap(version)]
struct Args {
    /// Where to put the patches
    #[clap(short, long, default_value = "out")]
    output_dir: Utf8PathBuf,

    /// Overwrite file if it already exists
    #[clap(long, default_value = "false")]
    overwrite: bool,

    /// Platform
    #[clap(short, long, default_value_t, value_enum)]
    platform: Platform,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    dbg!(&args);
    ensure!(
        args.output_dir.exists(),
        "output directory `{}` doesn't exist",
        args.output_dir
    );

    let (platform, extension) = match args.platform {
        Platform::MerisLvx => (8008, "syx"),
        Platform::Zoia => (3003, "bin"),
    };

    // reqwest client that retries failed requests
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(5);
    let client = ClientBuilder::new(Client::new())
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build();

    let paginated = PagedPatches {
        client: client.clone(),
    };
    let mut pager = std::pin::pin!(paginated.pages(GetPatchesRequest { platform, page: 1 }));
    while let Some(patches) = pager.try_next().await? {
        println!("Processing {} patches", patches.len());
        for patch in patches {
            println!("{patch:#?}");

            let mut filename = args.output_dir.join(&patch.slug);
            filename.set_extension(extension);

            if filename.exists() {
                if args.overwrite {
                    println!("Overwriting file: {filename}");
                } else {
                    println!("Retaining file: {filename}");
                    continue;
                }
            }

            let id = patch.id.as_u64().context("expected unsigned patch id")?;
            let metadata = get_patch_metadata(&client, id).await?;
            println!("{metadata:#?}");

            let patch_file = &metadata.files[0];
            let patch_file_extension = Path::new(&patch_file.filename)
                .extension()
                .and_then(OsStr::to_str);
            if patch_file_extension != Some(&extension) {
                println!("Skipping file: {}", patch_file.filename);
                continue;
            }

            let mut buf = get_patch_bytes(&client, &patch_file.url).await?;
            println!("Read {} bytes", buf.len());

            // TODO: Strategy
            if args.platform == Platform::MerisLvx {
                if let Some(filtered) = sysex_filter(&buf) {
                    buf = filtered.to_vec();
                    println!("Writing {} bytes", buf.len());
                } else {
                    println!("Nothing filtered.");
                }
            }

            let mut file = File::create(&filename)?;
            println!("Writing file: {filename}");
            file.write_all(&buf)?;
        }
    }
    Ok(())
}
