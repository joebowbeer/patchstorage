use camino::Utf8PathBuf;
use clap::{error::ErrorKind as ClapErrorKind, CommandFactory, Parser};
use error_chain::error_chain;
use futures_util::TryStreamExt;
use page_turner::prelude::*;
use parse_link_header::parse_with_rel;
use reqwest::header::{self, HeaderMap};
use serde::Deserialize;
use serde_json::Number;
use std::fs::File;
use std::io::Write;

error_chain! {
    foreign_links {
        Io(std::io::Error);
        HttpRequest(reqwest::Error);
    }
}

const MERIS_LVX_PLATFORM: u64 = 8008;

#[derive(Debug, Deserialize)]
struct Patch {
    id: Number,
    slug: String,
}

#[derive(Clone)]
struct GetPatchesRequest {
    platform: u64,
    page: u32,
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

struct PatchesClient {}

impl PatchesClient {
    async fn get_patches_page(&self, request: GetPatchesRequest) -> Result<PatchesPage> {
        let response = reqwest::get(request.build()).await?;
        let has_next = has_next(response.headers().clone());
        let patches = response.json::<Vec<Patch>>().await?;
        Ok(PatchesPage { patches, has_next })
    }
}

fn has_next(headers: HeaderMap) -> bool {
    let link_header = headers.get(header::LINK).unwrap().to_str().unwrap();
    parse_with_rel(link_header).unwrap().get("next").is_some()
}

impl PageTurner<GetPatchesRequest> for PatchesClient {
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

struct GetPatchMetaDataRequest {
    id: u64,
}

impl GetPatchMetaDataRequest {
    fn build(&self) -> String {
        format!("https://patchstorage.com/api/beta/patches/{}", self.id)
    }
}

async fn get_patch_metadata(request: GetPatchMetaDataRequest) -> Result<PatchMetaData> {
    let response = reqwest::get(request.build()).await?;
    let metadata = response.json::<PatchMetaData>().await?;
    Ok(metadata)
}

#[derive(Debug, Parser)]
#[clap(version)]
struct Args {
    /// Where to put the patches
    #[clap(short, long, default_value = "out")]
    output_dir: Utf8PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    dbg!(&args);
    if !args.output_dir.exists() {
        let mut cmd = Args::command();
        cmd.error(
            ClapErrorKind::ValueValidation,
            format!("output directory `{}` doesn't exist", args.output_dir),
        )
        .exit();
    }

    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()?;

    let fetcher = PatchesClient {};
    let mut pager = std::pin::pin!(fetcher.pages(GetPatchesRequest {
        platform: MERIS_LVX_PLATFORM,
        page: 1
    }));
    while let Some(patches) = pager.try_next().await? {
        for patch in patches {
            println!("{patch:#?}");

            let mut filename = args.output_dir.join(&patch.slug);
            filename.set_extension("syx");

            // TODO: option to overwrite
            if filename.exists() {
                println!("Skipping file: {filename}");
                break;
            }

            let request = GetPatchMetaDataRequest {
                id: patch.id.as_u64().unwrap(),
            };
            let metadata = get_patch_metadata(request).await?;
            println!("{metadata:#?}");

            // TODO: retry on failure
            let bytes = client
                .get(&metadata.files[0].url)
                .send()
                .await?
                .bytes()
                .await?;

            let mut file = File::create(&filename)?;
            println!("Writing file: {filename}");
            file.write_all(&bytes).unwrap();
        }
    }
    Ok(())
}
