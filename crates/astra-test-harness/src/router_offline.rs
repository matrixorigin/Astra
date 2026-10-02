//! Explicit local offline artifact boundary; never loads credentials or invokes
//! a provider, executor, replay service, or subprocess.
use anyhow::{Context, Result};
use astra_services::model_routing::offline::*;
use astra_services::tuning::{RouterQualificationProtocol, router_evaluation_plan_sha256};
use astra_turn_core::model_routing::offline::{RouterTrainingConfig, train_router};
use astra_turn_core::model_routing::qualification::{qualify_router, shadow_router};
use clap::Subcommand;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Subcommand)]
pub enum RouterCommand {
    /// Hash the manifest and outcome-free source roster for protocol registration.
    RouterPlanHash {
        #[arg(long)]
        input: PathBuf,
    },
    /// Hash the normalized training configuration for a qualification protocol.
    RouterConfigHash {
        #[arg(long)]
        config: Option<PathBuf>,
    },
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
    /// Apply a preregistered held-out gate; never activates runtime routing.
    RouterQualify {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        authorization: PathBuf,
        #[arg(long)]
        protocol: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Score separately authorized later traces after rechecking qualification.
    RouterShadow {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        authorization: PathBuf,
        #[arg(long)]
        protocol: PathBuf,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        shadow_input: PathBuf,
        #[arg(long)]
        shadow_authorization: PathBuf,
        #[arg(long)]
        output: PathBuf,
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
            Self::RouterPlanHash { input } => {
                let input: RouterDatasetInput = read_json(&input)?;
                println!(
                    "{}",
                    router_evaluation_plan_sha256(&input).map_err(anyhow::Error::msg)?
                );
                Ok(())
            }
            Self::RouterConfigHash { config } => {
                let config: RouterTrainingConfig = config
                    .as_deref()
                    .map(read_json)
                    .transpose()?
                    .unwrap_or_default();
                println!("{}", content_sha256(&config).map_err(anyhow::Error::msg)?);
                Ok(())
            }
            Self::RouterQualify {
                input,
                authorization,
                protocol,
                config,
                output,
            } => {
                let result = qualify_router(
                    read_json(&input)?,
                    &read_json(&authorization)?,
                    chrono::Utc::now(),
                    config
                        .as_deref()
                        .map(read_json)
                        .transpose()?
                        .unwrap_or_default(),
                    read_json::<RouterQualificationProtocol>(&protocol)?,
                )
                .map_err(anyhow::Error::msg)?;
                publish(
                    &output,
                    |directory| write_json(&directory.join("qualification.json"), &result),
                    &serde_json::json!({"schema_version":1, "artifact_sha256":content_sha256(&result).map_err(anyhow::Error::msg)?, "source_ids":result.tuning.source_ids, "expires_at":result.tuning.expires_at}),
                )?;
                println!(
                    "Qualification status: {:?}; production_qualified=false",
                    result.tuning.status
                );
                Ok(())
            }
            Self::RouterShadow {
                input,
                authorization,
                protocol,
                config,
                shadow_input,
                shadow_authorization,
                output,
            } => {
                let result = shadow_router(
                    read_json(&input)?,
                    &read_json(&authorization)?,
                    read_json(&shadow_input)?,
                    &read_json(&shadow_authorization)?,
                    chrono::Utc::now(),
                    config
                        .as_deref()
                        .map(read_json)
                        .transpose()?
                        .unwrap_or_default(),
                    read_json::<RouterQualificationProtocol>(&protocol)?,
                )
                .map_err(anyhow::Error::msg)?;
                publish(
                    &output,
                    |directory| write_json(&directory.join("shadow.json"), &result),
                    &serde_json::json!({"schema_version":1, "artifact_sha256":content_sha256(&result).map_err(anyhow::Error::msg)?, "source_ids":result.source_ids, "expires_at":result.expires_at}),
                )?;
                println!(
                    "Offline shadow scored {} decisions; runtime routing unchanged",
                    result.decisions.len()
                );
                Ok(())
            }
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
                publish(
                    &output,
                    |directory| {
                        write_json(&directory.join("manifest.json"), &result.dataset.manifest)?;
                        write_json(&directory.join("candidate.json"), &result.candidate)?;
                        write_json(&directory.join("report.json"), &result.report)?;
                        let mut examples = fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(directory.join("examples.jsonl"))?;
                        for example in &result.dataset.examples {
                            serde_json::to_writer(&mut examples, example)?;
                            examples.write_all(b"\n")?;
                        }
                        examples.sync_all()?;
                        Ok(())
                    },
                    &serde_json::json!({"schema_version":1, "dataset_sha256":result.dataset.content_sha256, "source_ids":result.dataset.source_ids(), "expires_at":result.dataset.manifest.expires_at}),
                )?;
                println!("Offline router artifacts written; production_qualified=false");
                Ok(())
            }
        }
    }
}

/// One publication boundary for training, qualification and shadow bundles.
fn publish(
    output: &Path,
    stage: impl FnOnce(&Path) -> Result<()>,
    completion: &impl Serialize,
) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    anyhow::ensure!(!output.exists(), "Output directory already exists");
    let temporary = tempfile::Builder::new()
        .prefix(".router-")
        .tempdir_in(parent)?;
    stage(temporary.path())?;
    let directory = fs::DirBuilder::new();
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::DirBuilderExt;
        let mut private = directory;
        private.mode(0o700);
        private
    };
    directory
        .create(output)
        .context("Reserve new routing output directory")?;
    for entry in fs::read_dir(temporary.path())? {
        let entry = entry?;
        fs::rename(entry.path(), output.join(entry.file_name()))?;
    }
    write_json(&output.join("complete.json"), completion)?;
    Ok(())
}
