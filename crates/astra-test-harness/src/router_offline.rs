//! Explicit local offline artifact boundary; never loads credentials or invokes
//! a provider, executor, replay service, or subprocess.
use anyhow::{Context, Result};
use astra_services::evaluation::router::*;
use astra_turn_core::model_routing::offline::{RouterTrainingConfig, train_router};
use clap::Subcommand;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Subcommand)]
pub enum RouterCommand {
    /// Compute source digests for independent consent/redaction review.
    RouterSourceHashes {
        #[arg(long)]
        input: PathBuf,
    },
    /// Build an approved dataset and train/evaluate an offline-only router.
    RouterOffline {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        authorization: PathBuf,
        /// New output directory; existing artifacts are never overwritten.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
    },
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    // Bound memory use and avoid echoing unredacted source content on errors.
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("Open offline routing input")?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 64 * 1024 * 1024,
        "Offline input exceeds 64 MiB"
    );
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid offline routing JSON schema"))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
impl RouterCommand {
    pub fn run(self) -> Result<()> {
        match self {
            Self::RouterSourceHashes { input } => {
                let input: RouterDatasetInput = read_json(&input)?;
                let mut hashes = std::collections::BTreeMap::new();
                for source in &input.sources {
                    anyhow::ensure!(
                        !hashes.contains_key(&source.source_id),
                        "Duplicate source identity"
                    );
                    hashes.insert(
                        &source.source_id,
                        content_sha256(source).map_err(anyhow::Error::msg)?,
                    );
                }
                println!("{}", serde_json::to_string_pretty(&hashes)?);
                Ok(())
            }
            Self::RouterOffline {
                input,
                authorization,
                output,
                config,
            } => {
                let input: RouterDatasetInput = read_json(&input)?;
                let auth: RouterDataAuthorization = read_json(&authorization)?;
                let config: RouterTrainingConfig = config
                    .as_deref()
                    .map(read_json)
                    .transpose()?
                    .unwrap_or_default();
                let result = train_router(input, &auth, chrono::Utc::now(), config)
                    .map_err(anyhow::Error::msg)?;
                // Stage serialization before publishing. The completion record is
                // written last; consumers must require it for a usable bundle.
                let parent = output
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                anyhow::ensure!(!output.exists(), "Output directory already exists");
                let temporary = tempfile::Builder::new()
                    .prefix(".router-")
                    .tempdir_in(parent)?;
                write_json(
                    &temporary.path().join("manifest.json"),
                    &result.dataset.manifest,
                )?;
                write_json(&temporary.path().join("candidate.json"), &result.candidate)?;
                write_json(&temporary.path().join("report.json"), &result.report)?;
                let mut examples = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(temporary.path().join("examples.jsonl"))?;
                for example in &result.dataset.examples {
                    serde_json::to_writer(&mut examples, example)?;
                    examples.write_all(b"\n")?;
                }
                examples.sync_all()?;
                // Reserve destination with create_dir (no replacement, including
                // symlinks). Move files only after successful serialization.
                let directory = fs::DirBuilder::new();
                #[cfg(unix)]
                let directory = {
                    use std::os::unix::fs::DirBuilderExt;
                    let mut private = directory;
                    private.mode(0o700);
                    private
                };
                directory
                    .create(&output)
                    .context("Reserve new routing output directory")?;
                for entry in fs::read_dir(temporary.path())? {
                    let entry = entry?;
                    fs::rename(entry.path(), output.join(entry.file_name()))?;
                }
                write_json(
                    &output.join("complete.json"),
                    &serde_json::json!({"schema_version":1, "dataset_sha256":result.dataset.content_sha256, "source_ids":result.dataset.source_ids(), "expires_at":result.dataset.manifest.expires_at}),
                )?;
                println!("Offline router artifacts written; production_qualified=false");
                Ok(())
            }
        }
    }
}
