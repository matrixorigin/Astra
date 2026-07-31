//! Fail-closed verifier for a Phase-0 production history-work artifact.

use std::fs;
use std::path::PathBuf;

use astra_core::history_work_baseline::{
    ProductionBaselineArtifact, verify_current_build_attestation,
};

#[derive(Debug, PartialEq, Eq)]
struct Options {
    input: PathBuf,
}

fn parse_options<I>(args: I) -> Result<Options, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let _program = args.next();
    let mut input = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--input" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--input requires a path".to_string())?;
                if input.replace(PathBuf::from(value)).is_some() {
                    return Err("--input may be supplied only once".to_string());
                }
            }
            "--help" | "-h" => {
                return Err(
                    "usage: history-work-production-baseline-verify --input <artifact.json>"
                        .to_string(),
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok(Options {
        input: input.ok_or_else(|| "--input is required".to_string())?,
    })
}

fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(&options.input)?;
    let artifact: ProductionBaselineArtifact = serde_json::from_slice(&bytes)?;
    let baseline_run_id = artifact
        .process_captures
        .first()
        .map(|capture| capture.baseline_run_id.as_str())
        .ok_or("production baseline artifact has no process captures")?;
    verify_current_build_attestation(&artifact.provenance.git_sha, baseline_run_id)?;
    artifact.verify()?;
    println!(
        "verified {} production scenarios and {} instrumented sites",
        artifact.scenarios.len(),
        artifact.site_totals.len()
    );
    Ok(())
}

fn main() {
    let result: Result<(), Box<dyn std::error::Error>> = match parse_options(std::env::args()) {
        Ok(options) => run(options),
        Err(error) => Err(error.into()),
    };
    if let Err(error) = result {
        eprintln!("production baseline verification failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_requires_one_explicit_input() {
        assert_eq!(
            parse_options([
                "verify".to_string(),
                "--input".to_string(),
                "a.json".to_string()
            ])
            .unwrap(),
            Options {
                input: PathBuf::from("a.json")
            }
        );
        assert!(parse_options(["verify".to_string()]).is_err());
        assert!(
            parse_options([
                "verify".to_string(),
                "--input".to_string(),
                "a.json".to_string(),
                "--input".to_string(),
                "b.json".to_string(),
            ])
            .is_err()
        );
    }
}
