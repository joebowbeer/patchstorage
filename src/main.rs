use anyhow::{ensure, Context, Error, Result};
use camino::Utf8PathBuf;
use clap::Parser;
use futures_util::TryStreamExt;
use httpclient::{header, header::HeaderMap, Client, InMemoryResponseExt, Retry};
use page_turner::prelude::*;
use parse_link_header::parse_with_rel;
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

struct PagedPatches {
    client: Client,
}

impl PagedPatches {
    async fn get_patches_page(&self, request: GetPatchesRequest) -> Result<PatchesPage> {
        let response = self.client.get(&request.build()).await?;
        let has_next = self.has_next(&response.headers())?;
        let patches = response.json::<Vec<Patch>>()?;
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

async fn get_patch_metadata(client: &Client, id: u64) -> Result<PatchMetaData> {
    let url = format!("https://patchstorage.com/api/beta/patches/{id}");
    let response = client.get(&url).await?;
    let metadata = response.json::<PatchMetaData>()?;
    Ok(metadata)
}

async fn get_patch_bytes(client: &Client, url: &str) -> Result<Vec<u8>> {
    let response = client.get(&url).await?;
    let bytes = response.bytes()?.to_vec();
    Ok(bytes)
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

    let client = Client::new().with_middleware(Retry::default());
    let paginated = PagedPatches {
        client: client.clone(),
    };
    let mut pager = std::pin::pin!(paginated.pages(GetPatchesRequest {
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

            let id = patch.id.as_u64().context("expected unsigned patch id")?;
            let metadata = get_patch_metadata(&client, id).await?;
            println!("{metadata:#?}");

            let buf = get_patch_bytes(&client, &metadata.files[0].url).await?;
            println!("Read {} bytes", buf.len());

            let mut file = File::create(&filename)?;
            println!("Writing file: {filename}");
            file.write_all(&buf)?;
        }
    }
    Ok(())
}
