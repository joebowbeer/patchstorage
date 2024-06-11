use anyhow::{ensure, Context, Error, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use futures_util::TryStreamExt;
use page_turner::prelude::*;
use parse_link_header::parse_with_rel;
use reqwest::header::{self, HeaderMap};
use serde::Deserialize;
use serde_json::Number;
use std::fs::File;
use std::io::Write;

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
        let has_next = has_next(response.headers().clone())?;
        let patches = response.json::<Vec<Patch>>().await?;
        Ok(PatchesPage { patches, has_next })
    }
}

fn has_next(headers: HeaderMap) -> Result<bool> {
    let link_header = headers
        .get(header::LINK)
        .context("missing Link header")?
        .to_str()?;
    let rel_map = parse_with_rel(link_header)?;
    Ok(rel_map.get("next").is_some())
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
    ensure!(
        args.output_dir.exists(),
        "output directory `{}` doesn't exist",
        args.output_dir
    );

    let fetcher = PatchesClient {};
    let mut pager = std::pin::pin!(fetcher.pages(GetPatchesRequest {
        platform: MERIS_LVX_PLATFORM,
        page: 1
    }));
    while let Some(patches) = pager.try_next().await? {
        println!("Processing {} patches", patches.len());
        for patch in patches {
            println!("{patch:#?}");

            let mut filename = args.output_dir.join(&patch.slug);
            filename.set_extension("syx");

            // TODO: option to overwrite
            if filename.exists() {
                println!("Skipping file: {filename}");
                continue;
            }

            let request = GetPatchMetaDataRequest {
                id: patch.id.as_u64().context("patch id is unsigned int")?,
            };
            let metadata = get_patch_metadata(request).await?;
            println!("{metadata:#?}");

            // TODO: retry on failure
            let bytes = reqwest::get(&metadata.files[0].url).await?.bytes().await?;

            let mut file = File::create(&filename)?;
            println!("Writing file: {filename}");
            file.write_all(&bytes)?;
        }
    }
    Ok(())
}
