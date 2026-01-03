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

/// Patchstorage endpoint
// https://patchstorage.com/docs/api/beta/
// https://github.com/patchstorage/patchstorage-docs/wiki
const PATCHSTORAGE_API: &str = "https://patchstorage.com/api/beta";

// TODO: Add dry-run, limit and search
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
        Platform::EventideH90 => (8271, "pgm90"), // extensions: pgm90, lst90, preset90
        Platform::MerisEnzoX => (10559, "syx"),
        Platform::MerisLvx => (8008, "syx"),
        Platform::MerisMercuryX => (9190, "syx"),
        Platform::Mozaic => (3341, "mozaic"), // extensions: mozaic, txt, zip
        Platform::Zoia => (3003, "bin"),      // extensions: bin, zip
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
            if !has_extension(&patch_file.filename, extension) {
                println!("Skipping file: {}", patch_file.filename);
                continue;
            }

            let mut buf = get_patch_bytes(&client, &patch_file.url).await?;
            println!("Read {} bytes", buf.len());

            if extension == "syx" {
                if let Some(filtered) = sysex_filter(&buf) {
                    buf = filtered.to_vec();
                    println!("Accepted {} bytes", buf.len());
                } else {
                    println!("Nothing trimmed");
                }
            }

            let mut file = File::create(&filename)?;
            println!("Writing file: {filename}");
            file.write_all(&buf)?;
        }
    }
    Ok(())
}

#[derive(clap::ValueEnum, Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Platform {
    /// Eventide H90
    EventideH90,
    /// Meris Enzo X
    MerisEnzoX,
    /// Meris LVX
    #[default]
    MerisLvx,
    /// Meris MercuryX
    MerisMercuryX,
    /// Mozaic
    Mozaic,
    /// ZOIA / Euroburo
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
            "{PATCHSTORAGE_API}/patches/?platforms={}&page={}",
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
        let response = self.client.get(request.build()).send().await?;
        let has_next = self.has_next(response.headers())?;
        let patches = response.json::<Vec<Patch>>().await?;
        Ok(PatchesPage { patches, has_next })
    }

    // TODO: Use x-wp-totalpages
    // https://developer.wordpress.org/rest-api/using-the-rest-api/pagination/
    fn has_next(&self, headers: &HeaderMap) -> Result<bool> {
        let link_header = headers
            .get(header::LINK)
            .context("missing Link header")?
            .to_str()?;
        Ok(parse_with_rel(link_header)?.contains_key("next"))
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
    let url = format!("{PATCHSTORAGE_API}/patches/{id}");
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
    let mut iter = buf.iter();
    let start = iter.position(|x| *x >= 0xF0)?;
    if buf[start] != 0xF0 {
        eprintln!("Unsupported system message '{:?}'", buf[start]);
        return None;
    }
    let last = iter.position(|x| *x == 0xF7)?;
    let end = start + last + 2;
    if (start, end) == (0, buf.len()) {
        // Unchanged
        return None;
    }
    Some(&buf[start..end])
}

#[test]
fn sysex_filter_test() {
    assert_eq!(sysex_filter(&[]), None);
    assert_eq!(sysex_filter(&[0xF0, 0xF7]), None);
    assert_eq!(
        sysex_filter(&[0xB0, 0x76, 0x7F, 0xF0, 0xF7]).unwrap().len(),
        2
    );
    assert_eq!(
        sysex_filter(&[0xF0, 0xF7, 0xB0, 0x76, 0x00]).unwrap().len(),
        2
    );
}

fn has_extension(filename: &str, extension: &str) -> bool {
    Path::new(filename).extension() == Some(OsStr::new(extension))
}

#[test]
fn has_extension_test() {
    assert!(!has_extension("basename", "bin"));
    assert!(!has_extension("basename.syx", "bin"));
    assert!(has_extension("basename.syx", "syx"));
    assert!(has_extension("basename.tar.gz", "gz"));
}
