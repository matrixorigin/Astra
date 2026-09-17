//! Advisory-only closure for executor-retained source preimages.
//!
//! The state is derived from the lossless tool ledger. It does not parse task
//! prose, shell commands, database formats, or model output. A matching,
//! successful receipt restore is the only event that resolves a source-change
//! fact; unrelated successful calls cannot accidentally clear it.

use std::collections::BTreeMap;

use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use serde_json::{Value, json};

fn restored_source_receipt(record: &ToolCallRecord) -> Option<String> {
    if record.name != "rollback_file_edits"
        || !record.ok
        || record.effective_disposition() != ToolCallDisposition::Executed
    {
        return None;
    }
    let args = serde_json::from_str::<Value>(record.authoritative_args_full()?).ok()?;
    let receipt_id = args.get("receipt_id").and_then(Value::as_str)?;
    if args.get("scope").and_then(Value::as_str) != Some("source_receipt") {
        return None;
    }
    let result = serde_json::from_str::<Value>(record.result_full.as_deref()?).ok()?;
    (result.get("success").and_then(Value::as_bool) == Some(true)
        && result.get("scope").and_then(Value::as_str) == Some("source_receipt")
        && result.get("receipt_id").and_then(Value::as_str) == Some(receipt_id))
    .then(|| receipt_id.to_string())
}

/// Project unresolved inferred-source changes into one compact model-facing
/// advisory. Returning `None` means there is no active recovery fact.
pub(crate) fn active_source_recovery_advisory(records: &[ToolCallRecord]) -> Option<Value> {
    let mut active = BTreeMap::<String, Vec<String>>::new();
    for record in records {
        if let Some(receipt_id) = restored_source_receipt(record) {
            active.remove(&receipt_id);
        }
        let Some(fact) = record.source_preimage_recovery.as_ref() else {
            continue;
        };
        let Some(receipt_id) = fact.get("receipt_id").and_then(Value::as_str) else {
            continue;
        };
        let Some(paths) = fact.get("changed_paths").and_then(Value::as_array) else {
            continue;
        };
        let paths = paths
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if !paths.is_empty() {
            active.insert(receipt_id.to_string(), paths);
        }
    }
    if active.is_empty() {
        return None;
    }
    let receipts = active
        .into_iter()
        .map(|(receipt_id, changed_paths)| {
            json!({
                "receipt_id": receipt_id,
                "changed_paths": changed_paths,
            })
        })
        .collect::<Vec<_>>();
    Some(json!({
        "signal": "source_preimage_recovery_pending",
        "status": "advisory",
        "receipts": receipts,
        "assessment": "A completed command modified or deleted source evidence whose original bytes were retained.",
        "recommendation": "Before reopening or transforming an affected source, restore the matching receipt with rollback_file_edits(scope=source_receipt, receipt_id=...), then work on a derived/disposable copy or explicitly declare protected source_artifacts. Repeating the same stateful observation against the restored original can destroy it again.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(receipt: &str, path: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: "bash".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            source_preimage_recovery: Some(json!({
                "schema_version": 1,
                "source": "astra_source_preimage_store",
                "receipt_id": receipt,
                "changed_paths": [path],
            })),
            ..Default::default()
        }
    }

    fn restore(receipt: &str, ok: bool) -> ToolCallRecord {
        ToolCallRecord {
            name: "rollback_file_edits".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            runtime_args_full: Some(
                json!({"scope": "source_receipt", "receipt_id": receipt}).to_string(),
            ),
            result_full: Some(
                json!({
                    "success": ok,
                    "scope": "source_receipt",
                    "receipt_id": receipt,
                })
                .to_string(),
            ),
            ..Default::default()
        }
    }

    #[test]
    fn unrelated_success_does_not_clear_source_recovery() {
        let ordinary = ToolCallRecord {
            name: "read_file".into(),
            ok: true,
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        };
        let advisory = active_source_recovery_advisory(&[
            changed("00000000-0000-4000-8000-000000000001", "input.bin"),
            ordinary,
        ])
        .expect("source recovery remains active");
        assert_eq!(advisory["receipts"][0]["changed_paths"][0], "input.bin");
    }

    #[test]
    fn only_matching_successful_restore_clears_and_a_new_change_reactivates() {
        let receipt_a = "00000000-0000-4000-8000-000000000001";
        let receipt_b = "00000000-0000-4000-8000-000000000002";
        assert!(
            active_source_recovery_advisory(&[
                changed(receipt_a, "input.bin"),
                restore(receipt_b, true),
                restore(receipt_a, false),
            ])
            .is_some()
        );
        assert!(
            active_source_recovery_advisory(&[
                changed(receipt_a, "input.bin"),
                restore(receipt_a, true),
            ])
            .is_none()
        );
        assert!(
            active_source_recovery_advisory(&[
                changed(receipt_a, "input.bin"),
                restore(receipt_a, true),
                changed(receipt_b, "input.bin"),
            ])
            .is_some()
        );
    }

    #[test]
    fn recovery_state_survives_the_durable_record_projection() {
        let receipt = "00000000-0000-4000-8000-000000000001";
        let mut restore = restore(receipt, true);
        restore.args_full = restore.runtime_args_full.take();
        let records = vec![changed(receipt, "input.bin"), restore];
        let encoded = serde_json::to_vec(&records).expect("records serialize");
        let restored: Vec<ToolCallRecord> =
            serde_json::from_slice(&encoded).expect("records restore");
        assert!(
            active_source_recovery_advisory(&restored).is_none(),
            "a successful matching restore must remain authoritative after checkpoint roundtrip"
        );
    }
}
