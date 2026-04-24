use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use astra_core::SkillSearchSettings;
use astra_runtime::skills::quality::SkillQualityTracker;
use astra_runtime::turn::skill_tool::{
    build_skill_selector_shortlist_trace, visible_skills_for_host_turn, InvokedSkill, SkillToolInfo,
};
use astra_skills::loader::load_skill_from_path;
use astra_turn_core::skill_selector_metrics::compute_skill_selector_metric;
use serde::{Deserialize, Serialize};

// Default paths are relative to the repository root and assume the corpus
// has been extracted via `tar -xzf benchmarks/skill-selector/skills.tar.gz
//   -C benchmarks/skill-selector/skills/` (see benchmarks/skill-selector/README.md).
const DEFAULT_SAMPLE_DIR: &str = "benchmarks/skill-selector/skills";
const DEFAULT_DATASET: &str = "benchmarks/skill-selector/dataset/primary.jsonl";
const DEFAULT_OUTPUT: &str = "benchmarks/skill-selector/results/primary-selector-run.jsonl";
const DEFAULT_SUMMARY: &str =
    "benchmarks/skill-selector/results/primary-selector-run-summary.json";

#[derive(Debug)]
struct Args {
    sample_dir: PathBuf,
    dataset: PathBuf,
    output: PathBuf,
    summary: PathBuf,
    limit: Option<usize>,
    include_unpassed: bool,
    concurrency: usize,
}

#[derive(Clone, Debug, Deserialize)]
struct PrimaryRecord {
    record_id: String,
    prompt_id: String,
    difficulty: Option<String>,
    target_skill: String,
    prompt: String,
    #[serde(default)]
    passes: bool,
}

#[derive(Debug, Serialize)]
struct ResultRow {
    record_id: String,
    prompt_id: String,
    difficulty: Option<String>,
    target_skill: String,
    visible_skill_count: i64,
    open_catalog: bool,
    best_rank: Option<i64>,
    hit_at_1: bool,
    hit_at_5: bool,
    hit_at_10: bool,
    hit_at_20: bool,
    top_skill: Option<String>,
    shortlist: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Summary {
    sample_dir: String,
    dataset: String,
    catalog_size: usize,
    total_records_in_dataset: usize,
    unique_target_skills_in_dataset: usize,
    skipped_unpassed_records: usize,
    evaluated_records: usize,
    hit_at_1_rate: f64,
    hit_at_5_rate: f64,
    hit_at_10_rate: f64,
    hit_at_20_rate: f64,
    avg_best_rank_on_hit: Option<f64>,
    misses_not_shortlisted: usize,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        sample_dir: PathBuf::from(DEFAULT_SAMPLE_DIR),
        dataset: PathBuf::from(DEFAULT_DATASET),
        output: PathBuf::from(DEFAULT_OUTPUT),
        summary: PathBuf::from(DEFAULT_SUMMARY),
        limit: None,
        include_unpassed: false,
        concurrency: 1,
    };

    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--concurrency" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--concurrency requires a value".to_string())?;
                args.concurrency = raw
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --concurrency value: {raw}"))?
                    .max(1);
            }
            "--sample-dir" => {
                args.sample_dir = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--sample-dir requires a value".to_string())?,
                );
            }
            "--dataset" => {
                args.dataset = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--dataset requires a value".to_string())?,
                );
            }
            "--output" => {
                args.output = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--output requires a value".to_string())?,
                );
            }
            "--summary" => {
                args.summary = PathBuf::from(
                    iter.next()
                        .ok_or_else(|| "--summary requires a value".to_string())?,
                );
            }
            "--limit" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--limit requires a value".to_string())?;
                args.limit = Some(
                    raw.parse::<usize>()
                        .map_err(|_| format!("invalid --limit value: {raw}"))?,
                );
            }
            "--include-unpassed" => args.include_unpassed = true,
            "--help" | "-h" => {
                return Err(
                    "usage: cargo run -- --limit 20 [--sample-dir PATH] [--dataset PATH] [--output PATH] [--summary PATH] [--include-unpassed]"
                        .to_string(),
                )
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(args)
}

fn loaded_to_tool_info(path: &Path) -> Result<SkillToolInfo, Box<dyn Error>> {
    let loaded = load_skill_from_path(path)?;
    let manifest = loaded.manifest;
    Ok(SkillToolInfo {
        name: manifest.name,
        description: manifest.description,
        when_to_use: manifest.when_to_use,
        source: manifest.source,
        aliases: manifest.aliases,
        category: manifest.category,
        tags: manifest.tags,
        triggers: manifest.triggers,
    })
}

fn load_catalog(sample_dir: &Path) -> Result<Vec<SkillToolInfo>, Box<dyn Error>> {
    let mut skills = Vec::new();
    for entry in fs::read_dir(sample_dir)? {
        let entry = entry?;
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }
        let skill_md = entry_path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let info = loaded_to_tool_info(&skill_md)?;
        skills.push(info);
    }
    Ok(skills)
}

fn load_records(dataset: &Path) -> Result<(usize, Vec<PrimaryRecord>), Box<dyn Error>> {
    let mut records = Vec::new();
    let mut index_by_target = HashMap::new();
    let mut raw_count = 0usize;
    let reader = BufReader::new(File::open(dataset)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        raw_count += 1;
        let record = serde_json::from_str::<PrimaryRecord>(&line)?;
        if let Some(existing_idx) = index_by_target.get(&record.target_skill).copied() {
            records[existing_idx] = record;
        } else {
            index_by_target.insert(record.target_skill.clone(), records.len());
            records.push(record);
        }
    }
    Ok((raw_count, records))
}

fn write_json<P: AsRef<Path>, T: Serialize>(path: P, value: &T) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

fn write_jsonl<P: AsRef<Path>, T: Serialize>(path: P, values: &[T]) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.as_ref().parent() {
        fs::create_dir_all(parent)?;
    }
    let mut writer = BufWriter::new(File::create(path)?);
    for value in values {
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(help_or_error) => {
            eprintln!("{help_or_error}");
            return if help_or_error.starts_with("usage:") {
                Ok(())
            } else {
                Err(help_or_error.into())
            };
        }
    };

    let embed_url = std::env::var("MEMORIA_EMBEDDING_BASE_URL").ok();
    let embed_key = std::env::var("MEMORIA_EMBEDDING_API_KEY").ok();
    match (embed_url.as_deref(), embed_key.as_deref()) {
        (Some(url), Some(_)) if !url.is_empty() => {
            eprintln!(
                "selector embedding fusion ENABLED via MEMORIA_EMBEDDING_* (model={})",
                std::env::var("MEMORIA_EMBEDDING_MODEL").unwrap_or_else(|_| "bge-m3".into())
            );
        }
        _ => eprintln!(
            "selector embedding fusion DISABLED (set MEMORIA_EMBEDDING_BASE_URL + MEMORIA_EMBEDDING_API_KEY to enable)"
        ),
    }

    // Embedding fusion in skill_selector requires a Tokio runtime; without it
    // the call silently falls back to lexical-only via `unwrap_or_default`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

async fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let catalog = std::sync::Arc::new(load_catalog(&fs::canonicalize(&args.sample_dir)?)?);
    let (raw_record_count, total_records) = load_records(&args.dataset)?;
    let tracker = std::sync::Arc::new(SkillQualityTracker::new());
    let search = std::sync::Arc::new(SkillSearchSettings::default());
    let pinned = std::sync::Arc::new(HashSet::<String>::new());
    let discovered = std::sync::Arc::new(HashSet::<String>::new());
    let invoked = std::sync::Arc::new(HashMap::<String, InvokedSkill>::new());

    let eligible: Vec<&PrimaryRecord> = total_records
        .iter()
        .filter(|r| args.include_unpassed || r.passes)
        .collect();
    let skipped_unpassed = total_records.len() - eligible.len();
    let to_run: Vec<PrimaryRecord> = eligible
        .into_iter()
        .take(args.limit.unwrap_or(usize::MAX))
        .cloned()
        .collect();

    eprintln!(
        "evaluating {} records with concurrency={}",
        to_run.len(),
        args.concurrency
    );

    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(args.concurrency));
    let mut tasks = futures::stream::FuturesUnordered::new();
    for record in to_run.into_iter() {
        let catalog = catalog.clone();
        let tracker = tracker.clone();
        let search = search.clone();
        let pinned = pinned.clone();
        let discovered = discovered.clone();
        let invoked = invoked.clone();
        let semaphore = semaphore.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            let _permit = futures::executor::block_on(semaphore.acquire_owned()).unwrap();
            let (visible, open_catalog, telemetry) = visible_skills_for_host_turn(
                &catalog,
                &record.prompt,
                &tracker,
                &pinned,
                &discovered,
                &invoked,
                &search,
            );
            let shortlist =
                build_skill_selector_shortlist_trace(&visible, open_catalog, telemetry);
            let metric = compute_skill_selector_metric(
                &shortlist,
                std::slice::from_ref(&record.target_skill),
            )
            .expect("metric");
            let h1 = metric.hit_at(1);
            let h5 = metric.hit_at(5);
            let h10 = metric.hit_at(10);
            let h20 = metric.hit_at(20);
            ResultRow {
                record_id: record.record_id.clone(),
                prompt_id: record.prompt_id.clone(),
                difficulty: record.difficulty.clone(),
                target_skill: record.target_skill.clone(),
                visible_skill_count: metric.visible_skill_count,
                open_catalog: shortlist.open_catalog,
                best_rank: metric.best_chosen_rank,
                hit_at_1: h1,
                hit_at_5: h5,
                hit_at_10: h10,
                hit_at_20: h20,
                top_skill: shortlist
                    .skills
                    .first()
                    .map(|e| e.skill_name.clone()),
                shortlist: shortlist
                    .skills
                    .iter()
                    .map(|e| e.skill_name.clone())
                    .collect(),
            }
        }));
    }

    use futures::StreamExt;
    let mut results = Vec::new();
    let mut hit1 = 0usize;
    let mut hit5 = 0usize;
    let mut hit10 = 0usize;
    let mut hit20 = 0usize;
    let mut best_rank_sum = 0f64;
    let mut best_rank_count = 0usize;
    let mut misses_not_shortlisted = 0usize;
    let mut done = 0usize;
    while let Some(joined) = tasks.next().await {
        let row = joined?;
        if row.hit_at_1 { hit1 += 1; }
        if row.hit_at_5 { hit5 += 1; }
        if row.hit_at_10 { hit10 += 1; }
        if row.hit_at_20 {
            hit20 += 1;
            if let Some(rank) = row.best_rank {
                best_rank_sum += rank as f64;
                best_rank_count += 1;
            }
        } else {
            misses_not_shortlisted += 1;
        }
        results.push(row);
        done += 1;
        if done % 50 == 0 {
            eprintln!("  progress {}", done);
        }
    }

    let evaluated = results.len();
    let denom = evaluated.max(1) as f64;
    let summary = Summary {
        sample_dir: args.sample_dir.display().to_string(),
        dataset: args.dataset.display().to_string(),
        catalog_size: catalog.len(),
        total_records_in_dataset: raw_record_count,
        unique_target_skills_in_dataset: total_records.len(),
        skipped_unpassed_records: skipped_unpassed,
        evaluated_records: evaluated,
        hit_at_1_rate: hit1 as f64 / denom,
        hit_at_5_rate: hit5 as f64 / denom,
        hit_at_10_rate: hit10 as f64 / denom,
        hit_at_20_rate: hit20 as f64 / denom,
        avg_best_rank_on_hit: if best_rank_count == 0 {
            None
        } else {
            Some(best_rank_sum / best_rank_count as f64)
        },
        misses_not_shortlisted,
    };

    write_jsonl(&args.output, &results)?;
    write_json(&args.summary, &summary)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
