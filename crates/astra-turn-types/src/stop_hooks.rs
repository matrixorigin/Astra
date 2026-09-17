//! Pure completion-check obligations shared by execution and checkpoints.

use crate::completion_settlement::deserialize_required_option;
use serde::{Deserialize, Serialize};

/// A verification command surfaced at the completion boundary.
/// Only authoritative hooks form a terminal contract; discovery is advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopHook {
    pub label: String,
    pub command: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub working_dir: Option<String>,
    pub depends_on: Vec<String>,
    /// Per-hook timeout hint, not authority to extend the run's deadline.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub timeout_secs: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub cache_key: Option<String>,
    pub authoritative: bool,
}

/// Exact hook obligations and consumed attempts at one execution frontier.
/// This contains neither request headers nor model credentials. Missing fields
/// must never become an empty obligation set during recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopHookObligations {
    pub stop_hooks: Vec<StopHook>,
    pub stop_hook_runs: u32,
    pub teammate_idle_hooks: Vec<StopHook>,
    pub teammate_idle_hook_runs: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_obligations_require_every_fact_including_nullable_fields() {
        let hook = StopHook {
            label: "verify".into(),
            command: "make check".into(),
            working_dir: None,
            depends_on: vec!["build".into()],
            timeout_secs: None,
            cache_key: None,
            authoritative: true,
        };
        let snapshot = StopHookObligations {
            stop_hooks: vec![hook.clone()],
            stop_hook_runs: 2,
            teammate_idle_hooks: vec![StopHook {
                authoritative: false,
                ..hook
            }],
            teammate_idle_hook_runs: 1,
        };
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_value::<StopHookObligations>(wire.clone()).unwrap(),
            snapshot
        );
        for field in wire.as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<StopHookObligations>(incomplete).is_err(),
                "missing {field}"
            );
        }
        for field in wire["stop_hooks"][0].as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete["stop_hooks"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<StopHookObligations>(incomplete).is_err(),
                "missing hook {field}"
            );
        }
        let mut extra = wire;
        extra["forward_headers"] = serde_json::json!({});
        assert!(serde_json::from_value::<StopHookObligations>(extra).is_err());
    }
}
