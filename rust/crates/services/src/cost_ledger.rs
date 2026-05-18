//! Durable session cost ledger primitives.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::models::PricingData;

const TOKENS_PER_MILLION: f64 = 1_000_000.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostLedgerEntry {
    pub session_id: String,
    pub turn_id: String,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

impl CostLedgerEntry {
    #[allow(clippy::too_many_arguments)]
    pub fn priced(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        agent_id: impl Into<String>,
        parent_agent_id: Option<String>,
        model: impl Into<String>,
        prompt_tokens: u64,
        completion_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        pricing: &PricingData,
    ) -> Result<Self, CostLedgerPricingError> {
        validate_rate("prompt", pricing.prompt)?;
        validate_rate("completion", pricing.completion)?;
        let cache_read_rate = match (cache_read_tokens, pricing.cache_read) {
            (0, _) => 0.0,
            (_, Some(rate)) => {
                validate_rate("cache_read", rate)?;
                rate
            }
            (_, None) => return Err(CostLedgerPricingError::MissingCacheReadRate),
        };
        let cache_write_rate = match (cache_write_tokens, pricing.cache_write) {
            (0, _) => 0.0,
            (_, Some(rate)) => {
                validate_rate("cache_write", rate)?;
                rate
            }
            (_, None) => return Err(CostLedgerPricingError::MissingCacheWriteRate),
        };

        let cost_usd = price_tokens(prompt_tokens, pricing.prompt)
            + price_tokens(completion_tokens, pricing.completion)
            + price_tokens(cache_read_tokens, cache_read_rate)
            + price_tokens(cache_write_tokens, cache_write_rate);

        Ok(Self {
            session_id: session_id.into(),
            turn_id: turn_id.into(),
            agent_id: agent_id.into(),
            parent_agent_id,
            model: model.into(),
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cost_usd,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CostLedger {
    session_id: String,
    entries: Vec<CostLedgerEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostLedgerSummary {
    pub total_cost_usd: f64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub per_model_cost_usd: BTreeMap<String, f64>,
    pub rolled_up_agent_cost_usd: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BudgetStatus {
    pub budget_usd: f64,
    pub spent_usd: f64,
    pub remaining_usd: f64,
    pub exceeded: bool,
}

impl CostLedger {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            entries: Vec::new(),
        }
    }

    pub fn append(&mut self, entry: CostLedgerEntry) -> Result<(), CostLedgerError> {
        if entry.session_id != self.session_id {
            return Err(CostLedgerError::SessionMismatch {
                ledger_session_id: self.session_id.clone(),
                entry_session_id: entry.session_id,
            });
        }
        if self.entries.iter().any(|existing| {
            existing.turn_id == entry.turn_id && existing.agent_id == entry.agent_id
        }) {
            return Err(CostLedgerError::DuplicateEntry {
                turn_id: entry.turn_id,
                agent_id: entry.agent_id,
            });
        }
        self.entries.push(entry);
        Ok(())
    }

    #[must_use]
    pub fn entries(&self) -> &[CostLedgerEntry] {
        &self.entries
    }

    #[must_use]
    pub fn summary(&self) -> CostLedgerSummary {
        let mut total_cost_usd = 0.0;
        let mut total_prompt_tokens: u64 = 0;
        let mut total_completion_tokens: u64 = 0;
        let mut total_cache_read_tokens: u64 = 0;
        let mut total_cache_write_tokens: u64 = 0;
        let mut per_model_cost_usd = BTreeMap::new();
        let mut rolled_up_agent_cost_usd = BTreeMap::new();
        for entry in &self.entries {
            total_cost_usd += entry.cost_usd;
            total_prompt_tokens = total_prompt_tokens.saturating_add(entry.prompt_tokens);
            total_completion_tokens =
                total_completion_tokens.saturating_add(entry.completion_tokens);
            total_cache_read_tokens =
                total_cache_read_tokens.saturating_add(entry.cache_read_tokens);
            total_cache_write_tokens =
                total_cache_write_tokens.saturating_add(entry.cache_write_tokens);
            *per_model_cost_usd.entry(entry.model.clone()).or_insert(0.0) += entry.cost_usd;
            *rolled_up_agent_cost_usd
                .entry(entry.agent_id.clone())
                .or_insert(0.0) += entry.cost_usd;
            if let Some(parent) = &entry.parent_agent_id {
                *rolled_up_agent_cost_usd.entry(parent.clone()).or_insert(0.0) += entry.cost_usd;
            }
        }
        CostLedgerSummary {
            total_cost_usd,
            total_prompt_tokens,
            total_completion_tokens,
            total_cache_read_tokens,
            total_cache_write_tokens,
            per_model_cost_usd,
            rolled_up_agent_cost_usd,
        }
    }

    pub fn to_json_lines(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn save_to_path(&self, path: &Path) -> Result<(), CostLedgerStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CostLedgerStoreError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let content = self
            .to_json_lines()
            .map_err(CostLedgerStoreError::Serialize)?;
        std::fs::write(path, content).map_err(|source| CostLedgerStoreError::Write {
            path: path.to_path_buf(),
            source,
        })?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(|source| CostLedgerStoreError::Open {
                path: path.to_path_buf(),
                source,
            })?;
        file.sync_data()
            .map_err(|source| CostLedgerStoreError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(())
    }

    pub fn load_from_path(
        session_id: impl Into<String>,
        path: &Path,
    ) -> Result<Self, CostLedgerLoadError> {
        let path_buf = path.to_path_buf();
        let content =
            std::fs::read_to_string(path).map_err(|source| CostLedgerLoadError::Read {
                path: path_buf,
                source,
            })?;
        Self::from_json_lines(session_id, &content)
    }

    #[must_use]
    pub fn budget_status(&self, budget_usd: f64) -> BudgetStatus {
        let spent_usd = self.summary().total_cost_usd;
        BudgetStatus {
            budget_usd,
            spent_usd,
            remaining_usd: (budget_usd - spent_usd).max(0.0),
            exceeded: spent_usd > budget_usd,
        }
    }

    pub fn from_json_lines(
        session_id: impl Into<String>,
        lines: &str,
    ) -> Result<Self, CostLedgerLoadError> {
        let mut ledger = Self::new(session_id);
        for (idx, line) in lines.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: CostLedgerEntry =
                serde_json::from_str(trimmed).map_err(|source| CostLedgerLoadError::Parse {
                    line: idx + 1,
                    source,
                })?;
            ledger
                .append(entry)
                .map_err(|source| CostLedgerLoadError::Ledger {
                    line: idx + 1,
                    source,
                })?;
        }
        Ok(ledger)
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum CostLedgerError {
    #[error(
        "entry session '{entry_session_id}' does not match ledger session '{ledger_session_id}'"
    )]
    SessionMismatch {
        ledger_session_id: String,
        entry_session_id: String,
    },
    #[error("duplicate cost entry for turn '{turn_id}' and agent '{agent_id}'")]
    DuplicateEntry { turn_id: String, agent_id: String },
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum CostLedgerPricingError {
    #[error("pricing field '{field}' must be finite and non-negative")]
    InvalidRate { field: &'static str },
    #[error("cache_read pricing is required when cache_read_tokens > 0")]
    MissingCacheReadRate,
    #[error("cache_write pricing is required when cache_write_tokens > 0")]
    MissingCacheWriteRate,
}

#[derive(Debug, thiserror::Error)]
pub enum CostLedgerLoadError {
    #[error("failed to read cost ledger '{path}': {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse cost ledger line {line}: {source}")]
    Parse {
        line: usize,
        source: serde_json::Error,
    },
    #[error("invalid cost ledger entry at line {line}: {source}")]
    Ledger {
        line: usize,
        source: CostLedgerError,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CostLedgerStoreError {
    #[error("failed to create cost ledger directory '{path}': {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to serialize cost ledger: {0}")]
    Serialize(serde_json::Error),
    #[error("failed to write cost ledger '{path}': {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to open cost ledger '{path}': {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn validate_rate(field: &'static str, rate: f64) -> Result<(), CostLedgerPricingError> {
    if !rate.is_finite() || rate < 0.0 {
        return Err(CostLedgerPricingError::InvalidRate { field });
    }
    Ok(())
}

fn price_tokens(tokens: u64, rate_per_million: f64) -> f64 {
    (tokens as f64 / TOKENS_PER_MILLION) * rate_per_million
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        turn_id: &str,
        agent_id: &str,
        parent: Option<&str>,
        model: &str,
        cost: f64,
    ) -> CostLedgerEntry {
        CostLedgerEntry {
            session_id: "s1".into(),
            turn_id: turn_id.into(),
            agent_id: agent_id.into(),
            parent_agent_id: parent.map(str::to_string),
            model: model.into(),
            prompt_tokens: 100,
            completion_tokens: 25,
            cache_read_tokens: 10,
            cache_write_tokens: 5,
            cost_usd: cost,
        }
    }

    #[test]
    fn ledger_persists_and_restores_json_lines() {
        let mut ledger = CostLedger::new("s1");
        ledger
            .append(entry("t1", "root", None, "claude", 0.10))
            .unwrap();
        ledger
            .append(entry("t2", "child", Some("root"), "claude", 0.25))
            .unwrap();

        let restored = CostLedger::from_json_lines("s1", &ledger.to_json_lines().unwrap()).unwrap();
        assert_eq!(restored.entries(), ledger.entries());
    }

    #[test]
    fn ledger_persists_to_disk_and_loads_for_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cost/ledger.jsonl");
        let mut ledger = CostLedger::new("s1");
        ledger
            .append(entry("t1", "root", None, "claude", 0.10))
            .unwrap();

        ledger.save_to_path(&path).unwrap();
        let restored = CostLedger::load_from_path("s1", &path).unwrap();

        assert_eq!(restored.entries(), ledger.entries());
    }

    #[test]
    fn summary_rolls_child_cost_into_parent_agent() {
        let mut ledger = CostLedger::new("s1");
        ledger
            .append(entry("t1", "root", None, "claude", 0.10))
            .unwrap();
        ledger
            .append(entry("t2", "child", Some("root"), "gpt", 0.25))
            .unwrap();

        let summary = ledger.summary();
        assert_eq!(summary.total_prompt_tokens, 200);
        assert_eq!(summary.total_cache_read_tokens, 20);
        assert_eq!(summary.total_cache_write_tokens, 10);
        assert_eq!(summary.per_model_cost_usd["claude"], 0.10);
        assert_eq!(summary.per_model_cost_usd["gpt"], 0.25);
        assert_eq!(summary.rolled_up_agent_cost_usd["child"], 0.25);
        assert_eq!(summary.rolled_up_agent_cost_usd["root"], 0.35);
    }

    #[test]
    fn duplicate_turn_agent_entries_are_rejected() {
        let mut ledger = CostLedger::new("s1");
        ledger
            .append(entry("t1", "root", None, "claude", 0.10))
            .unwrap();
        assert!(matches!(
            ledger.append(entry("t1", "root", None, "claude", 0.10)),
            Err(CostLedgerError::DuplicateEntry { .. })
        ));
    }

    #[test]
    fn budget_status_reports_exceeded_budget() {
        let mut ledger = CostLedger::new("s1");
        ledger
            .append(entry("t1", "root", None, "claude", 0.10))
            .unwrap();
        ledger
            .append(entry("t2", "child", Some("root"), "claude", 0.25))
            .unwrap();

        let status = ledger.budget_status(0.20);
        assert!(status.exceeded);
        assert_eq!(status.remaining_usd, 0.0);
    }

    #[test]
    fn priced_entry_computes_prompt_completion_and_cache_costs() {
        let pricing = PricingData {
            prompt: 2.0,
            completion: 8.0,
            cache_read: Some(0.5),
            cache_write: Some(1.5),
        };
        let entry = CostLedgerEntry::priced(
            "s1",
            "t1",
            "root",
            None,
            "claude",
            1_000_000,
            500_000,
            2_000_000,
            1_000_000,
            &pricing,
        )
        .unwrap();

        assert!((entry.cost_usd - 8.5).abs() < 0.000_001, "{entry:?}");
    }

    #[test]
    fn priced_entry_rejects_missing_cache_rate() {
        let pricing = PricingData {
            prompt: 2.0,
            completion: 8.0,
            cache_read: None,
            cache_write: Some(1.5),
        };
        assert_eq!(
            CostLedgerEntry::priced(
                "s1",
                "t1",
                "root",
                None,
                "claude",
                1,
                1,
                10,
                0,
                &pricing,
            ),
            Err(CostLedgerPricingError::MissingCacheReadRate)
        );
    }

    #[test]
    fn priced_entry_rejects_invalid_rates() {
        for (field, pricing) in [
            (
                "prompt",
                PricingData {
                    prompt: f64::NAN,
                    completion: 8.0,
                    cache_read: Some(0.5),
                    cache_write: Some(1.5),
                },
            ),
            (
                "completion",
                PricingData {
                    prompt: 2.0,
                    completion: f64::INFINITY,
                    cache_read: Some(0.5),
                    cache_write: Some(1.5),
                },
            ),
            (
                "cache_read",
                PricingData {
                    prompt: 2.0,
                    completion: 8.0,
                    cache_read: Some(-0.5),
                    cache_write: Some(1.5),
                },
            ),
            (
                "cache_write",
                PricingData {
                    prompt: 2.0,
                    completion: 8.0,
                    cache_read: Some(0.5),
                    cache_write: Some(f64::NEG_INFINITY),
                },
            ),
        ] {
            assert_eq!(
                CostLedgerEntry::priced(
                    "s1", "t1", "root", None, "claude", 1, 1, 1, 1, &pricing
                ),
                Err(CostLedgerPricingError::InvalidRate { field }),
                "{field}"
            );
        }
    }
}
