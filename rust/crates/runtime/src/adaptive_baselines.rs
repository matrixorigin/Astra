//! Adaptive baseline store — persists promoted experiment winners and reapplies
//! them as durable per-scope baselines.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::ab_testing::{Experiment, apply_config_diffs};
use crate::pipeline::routing::{DomainHint, TaskType, domain_hint_to_label};
use crate::runtime_config::RuntimeConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdaptiveBaselineScope {
    pub task_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

impl AdaptiveBaselineScope {
    pub fn for_routing(task_type: TaskType, domain: Option<DomainHint>) -> Self {
        Self {
            task_type: task_type_label(task_type).to_string(),
            domain: domain.map(|value| domain_hint_to_label(value).to_string()),
        }
    }

    pub fn from_experiment(experiment: &Experiment) -> Option<Self> {
        let task_type = experiment
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("task_type:"))?
            .to_string();
        let domain = experiment
            .tags
            .iter()
            .find_map(|tag| tag.strip_prefix("domain:"))
            .and_then(|value| (value != "any").then(|| value.to_string()));
        Some(Self { task_type, domain })
    }

    fn key(&self) -> String {
        format!(
            "{}::{}",
            self.task_type,
            self.domain.as_deref().unwrap_or("any")
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveBaseline {
    pub scope: AdaptiveBaselineScope,
    pub experiment_id: String,
    pub variant_id: String,
    pub promoted_at: SystemTime,
    pub config_diff: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AdaptiveBaselineSnapshot {
    active: HashMap<String, AdaptiveBaseline>,
    history: HashMap<String, Vec<AdaptiveBaseline>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveBaselinePromotion {
    pub scope: AdaptiveBaselineScope,
    pub experiment_id: String,
    pub variant_id: String,
    pub replaced_existing: bool,
    pub config_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdaptiveBaselineRollback {
    pub scope: AdaptiveBaselineScope,
    pub removed_variant_id: String,
    pub restored_variant_id: Option<String>,
}

pub struct AdaptiveBaselineStore {
    baselines: RwLock<AdaptiveBaselineSnapshot>,
    storage_path: Option<PathBuf>,
}

impl Default for AdaptiveBaselineStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveBaselineStore {
    pub fn new() -> Self {
        Self {
            baselines: RwLock::new(AdaptiveBaselineSnapshot::default()),
            storage_path: None,
        }
    }

    pub fn with_storage(path: PathBuf) -> Self {
        let snapshot = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|data| serde_json::from_str::<AdaptiveBaselineSnapshot>(&data).ok())
                .unwrap_or_default()
        } else {
            AdaptiveBaselineSnapshot::default()
        };

        Self {
            baselines: RwLock::new(snapshot),
            storage_path: Some(path),
        }
    }

    pub fn promote_winner(
        &self,
        experiment: &Experiment,
        winner_variant_id: &str,
    ) -> Result<Option<AdaptiveBaselinePromotion>, String> {
        let winner = experiment.variant(winner_variant_id).ok_or_else(|| {
            format!(
                "experiment {} missing winner variant {winner_variant_id}",
                experiment.id
            )
        })?;
        if winner.is_control || winner.config_diff.is_empty() {
            return Ok(None);
        }
        let scope = AdaptiveBaselineScope::from_experiment(experiment)
            .ok_or_else(|| format!("experiment {} missing baseline scope tags", experiment.id))?;
        let baseline = AdaptiveBaseline {
            scope: scope.clone(),
            experiment_id: experiment.id.clone(),
            variant_id: winner.id.clone(),
            promoted_at: SystemTime::now(),
            config_diff: winner.config_diff.clone(),
        };

        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let key = scope.key();
        let replaced = snapshot.active.insert(key.clone(), baseline);
        if let Some(previous) = replaced.clone() {
            snapshot.history.entry(key).or_default().push(previous);
        }
        self.persist(&snapshot);

        let mut config_keys = winner.config_diff.keys().cloned().collect::<Vec<_>>();
        config_keys.sort();

        Ok(Some(AdaptiveBaselinePromotion {
            scope,
            experiment_id: experiment.id.clone(),
            variant_id: winner.id.clone(),
            replaced_existing: replaced.is_some(),
            config_keys,
        }))
    }

    pub fn resolve(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) -> Option<AdaptiveBaseline> {
        let scope = AdaptiveBaselineScope::for_routing(task_type, domain);
        let snapshot = self.baselines.read().unwrap_or_else(|e| e.into_inner());
        snapshot.active.get(&scope.key()).cloned().or_else(|| {
            scope.domain.as_ref()?;
            let fallback = AdaptiveBaselineScope {
                task_type: scope.task_type,
                domain: None,
            };
            snapshot.active.get(&fallback.key()).cloned()
        })
    }

    pub fn apply_to_config(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
        config: &mut RuntimeConfig,
    ) -> Option<AdaptiveBaseline> {
        let baseline = self.resolve(task_type, domain)?;
        apply_config_diffs(config, &baseline.config_diff);
        Some(baseline)
    }

    pub fn rollback(
        &self,
        task_type: TaskType,
        domain: Option<DomainHint>,
    ) -> Option<AdaptiveBaselineRollback> {
        let scope = AdaptiveBaselineScope::for_routing(task_type, domain);
        let key = scope.key();
        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let removed = snapshot.active.remove(&key)?;
        let restored = snapshot
            .history
            .get_mut(&key)
            .and_then(|history| history.pop());
        if let Some(previous) = restored.clone() {
            snapshot.active.insert(key.clone(), previous);
        }
        if snapshot
            .history
            .get(&key)
            .is_some_and(|history| history.is_empty())
        {
            snapshot.history.remove(&key);
        }
        self.persist(&snapshot);

        Some(AdaptiveBaselineRollback {
            scope,
            removed_variant_id: removed.variant_id,
            restored_variant_id: restored.map(|baseline| baseline.variant_id),
        })
    }

    /// Rollback all baselines promoted from a specific experiment.
    ///
    /// Returns the list of rollbacks performed (one per scope where the
    /// experiment had a promoted baseline).
    pub fn rollback_experiment(&self, experiment_id: &str) -> Vec<AdaptiveBaselineRollback> {
        let mut snapshot = self.baselines.write().unwrap_or_else(|e| e.into_inner());
        let mut rollbacks = Vec::new();

        // Find all active baselines belonging to this experiment.
        let keys_to_rollback: Vec<String> = snapshot
            .active
            .iter()
            .filter(|(_, b)| b.experiment_id == experiment_id)
            .map(|(k, _)| k.clone())
            .collect();

        for key in keys_to_rollback {
            let Some(removed) = snapshot.active.remove(&key) else {
                continue;
            };
            let restored = snapshot
                .history
                .get_mut(&key)
                .and_then(|history| history.pop());
            if let Some(previous) = restored.clone() {
                snapshot.active.insert(key.clone(), previous);
            }
            if snapshot
                .history
                .get(&key)
                .is_some_and(|history| history.is_empty())
            {
                snapshot.history.remove(&key);
            }
            rollbacks.push(AdaptiveBaselineRollback {
                scope: removed.scope.clone(),
                removed_variant_id: removed.variant_id,
                restored_variant_id: restored.map(|b| b.variant_id),
            });
        }

        if !rollbacks.is_empty() {
            self.persist(&snapshot);
        }
        rollbacks
    }

    fn persist(&self, snapshot: &AdaptiveBaselineSnapshot) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                eprintln!("[adaptive-baselines] failed to create storage directory: {err}");
                return;
            }
        }
        let Ok(data) = serde_json::to_string_pretty(snapshot) else {
            return;
        };
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, data) {
            eprintln!("[adaptive-baselines] failed to write temp file: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            eprintln!("[adaptive-baselines] failed to rename temp file: {err}");
        }
    }
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::Code => "code",
        TaskType::Reasoning => "reasoning",
        TaskType::Fetch => "fetch",
        TaskType::Mutate => "mutate",
        TaskType::Memory => "memory",
        TaskType::Conversational => "conversational",
        TaskType::Compound => "compound",
        TaskType::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ab_testing::Variant;

    #[test]
    fn promote_and_resolve_baseline() {
        let store = AdaptiveBaselineStore::new();
        let experiment = Experiment::new("exp-fetch")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(8)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();

        let promotion = store
            .promote_winner(&experiment, "treatment")
            .unwrap()
            .expect("promotion");
        assert_eq!(promotion.scope.task_type, "fetch");

        let baseline = store
            .resolve(TaskType::Fetch, Some(DomainHint::Code))
            .expect("baseline");
        assert_eq!(baseline.variant_id, "treatment");
    }

    #[test]
    fn rollback_restores_previous_baseline() {
        let store = AdaptiveBaselineStore::new();
        let first = Experiment::new("exp-one")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-a")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(7)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        let second = Experiment::new("exp-two")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-b")
                    .with_traffic(0.5)
                    .with_config_diff("memory.retrieval_top_k", serde_json::json!(9)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();

        store.promote_winner(&first, "treatment-a").unwrap();
        store.promote_winner(&second, "treatment-b").unwrap();

        let rollback = store
            .rollback(TaskType::Fetch, None)
            .expect("rollback should restore previous baseline");
        assert_eq!(rollback.removed_variant_id, "treatment-b");
        assert_eq!(rollback.restored_variant_id.as_deref(), Some("treatment-a"));
    }

    #[test]
    fn persists_promoted_baselines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("adaptive-baselines.json");
        let experiment = Experiment::new("exp-persist")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment")
                    .with_traffic(0.5)
                    .with_config_diff("compression.max_history_tokens", serde_json::json!(28000)),
            )
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();

        let store = AdaptiveBaselineStore::with_storage(path.clone());
        store.promote_winner(&experiment, "treatment").unwrap();

        let restored = AdaptiveBaselineStore::with_storage(path);
        let baseline = restored.resolve(TaskType::Code, None).expect("restored");
        assert_eq!(baseline.variant_id, "treatment");
        assert_eq!(
            baseline.config_diff.get("compression.max_history_tokens"),
            Some(&serde_json::json!(28000))
        );
    }

    #[test]
    fn rollback_experiment_removes_all_matching_baselines() {
        let store = AdaptiveBaselineStore::new();

        // Two experiments — one promoted for Code, one for Fetch.
        let exp_a = Experiment::new("exp-a")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-a")
                    .with_traffic(0.5)
                    .with_config_diff("max_tools", serde_json::json!(50)),
            )
            .with_tag("task_type:code")
            .with_tag("domain:any")
            .build();
        store.promote_winner(&exp_a, "treatment-a");

        let exp_b = Experiment::new("exp-b")
            .with_variant(Variant::control())
            .with_variant(
                Variant::new("treatment-b")
                    .with_traffic(0.5)
                    .with_config_diff("max_tools", serde_json::json!(60)),
            )
            .with_tag("task_type:fetch")
            .with_tag("domain:any")
            .build();
        store.promote_winner(&exp_b, "treatment-b");

        // Both are active.
        assert!(store.resolve(TaskType::Code, None).is_some());
        assert!(store.resolve(TaskType::Fetch, None).is_some());

        // Rollback experiment-a only.
        let rollbacks = store.rollback_experiment("exp-a");
        assert_eq!(rollbacks.len(), 1);
        assert_eq!(rollbacks[0].removed_variant_id, "treatment-a");

        // Code baseline is gone, Fetch is untouched.
        assert!(store.resolve(TaskType::Code, None).is_none());
        assert!(store.resolve(TaskType::Fetch, None).is_some());
    }

    #[test]
    fn rollback_experiment_no_match_returns_empty() {
        let store = AdaptiveBaselineStore::new();
        let rollbacks = store.rollback_experiment("no-such-experiment");
        assert!(rollbacks.is_empty());
    }
}
