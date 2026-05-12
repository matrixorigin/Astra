//! Step 2a contract: HeavyCheckpoint carries a `config_version_id`
//! pointer to the `astra-config` version store. The pointer is the
//! single small field that lets `astra audit`/`astra session show`
//! answer "which config did this session run?" without dragging a
//! TOML blob into every checkpoint file.
//!
//! Kept in astra-pipeline (not astra-config) because:
//!   * the field lives on HeavyCheckpoint which is a pipeline type;
//!   * astra-pipeline intentionally does not depend on astra-config
//!     to avoid a crate cycle — so the pointer is `Option<String>`,
//!     opaque to this crate. Consumers that want the RuntimeConfig
//!     itself look it up in the store via the id.
//!
//! What this locks in:
//!
//! 1. The field exists, is `Option<String>`, defaults to `None`.
//! 2. Serde roundtrip with a set value preserves the pointer.
//! 3. Serde is backward-compatible: a checkpoint JSON from before
//!    this field existed still deserializes (field absent = None).

use astra_pipeline::step_protocol::{ExecutionCursor, HeavyCheckpoint, LightCheckpoint};

fn sample_light() -> LightCheckpoint {
    // LightCheckpoint doesn't derive Default (it carries an explicit
    // protocol_version and id fields that callers should not forget
    // to set). Build one by hand with the minimum fields; ExecutionCursor
    // does provide Default so we reuse that.
    LightCheckpoint {
        protocol_version: 0,
        cursor: ExecutionCursor::default(),
        step_id: "step_test".into(),
        task_id: "task_test".into(),
        agent_id: "agent_test".into(),
        progress: 0.0,
        total_tokens: 0,
        created_at: 0,
    }
}

fn sample_heavy_with_pointer(id: Option<String>) -> HeavyCheckpoint {
    HeavyCheckpoint {
        light: sample_light(),
        messages: Vec::new(),
        budget_remaining_tokens: 0,
        budget_remaining_rounds: 0,
        blocked_tools: Vec::new(),
        recent_tools: Vec::new(),
        memory_context: None,
        delegation_id: None,
        delegation_pattern: None,
        delegation_sub_run_summaries: Vec::new(),
        interruption: None,
        approval_overrides: None,
        consecutive_context_window_errors: 0,
        pipeline_state: None,
        compaction_state: None,
        config_version_id: id,
    }
}

#[test]
fn heavy_checkpoint_accepts_config_version_id_field() {
    let cp = sample_heavy_with_pointer(Some("cfg_abc0123456789def".to_string()));
    assert_eq!(
        cp.config_version_id.as_deref(),
        Some("cfg_abc0123456789def")
    );
}

#[test]
fn heavy_checkpoint_defaults_to_none_for_config_version_id() {
    // Legacy call sites that construct HeavyCheckpoint without
    // threading a version id through must still compile and deserialize.
    let cp = sample_heavy_with_pointer(None);
    assert!(cp.config_version_id.is_none());
}

#[test]
fn heavy_checkpoint_serde_roundtrip_preserves_pointer() {
    let cp = sample_heavy_with_pointer(Some("cfg_roundtrip1234567".to_string()));
    let ser = serde_json::to_string(&cp).expect("serialize");
    let de: HeavyCheckpoint = serde_json::from_str(&ser).expect("deserialize");
    assert_eq!(
        de.config_version_id.as_deref(),
        Some("cfg_roundtrip1234567")
    );
}

#[test]
fn heavy_checkpoint_deserializes_pre_field_json_without_error() {
    // Old checkpoint files on disk don't carry the new key. The field
    // must be `#[serde(default)]` so loading those files still works.
    // Build a JSON body that has the full pre-change schema, minus
    // only the new config_version_id field — that's the regression
    // we're guarding.
    let pre = sample_heavy_with_pointer(Some("cfg_toremove".into()));
    let full = serde_json::to_value(&pre).unwrap();
    // Drop the new field to simulate an old on-disk checkpoint.
    let mut obj = match full {
        serde_json::Value::Object(m) => m,
        _ => unreachable!(),
    };
    obj.remove("config_version_id");
    let legacy = serde_json::Value::Object(obj);

    let de: HeavyCheckpoint =
        serde_json::from_value(legacy).expect("pre-field JSON must still deserialize");
    assert!(
        de.config_version_id.is_none(),
        "missing field must default to None"
    );
}
