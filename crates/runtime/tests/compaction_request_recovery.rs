//! Historical failed rewrites are diagnostics, not a permanent session lock.
use astra_runtime::turn::compaction_replay::{
    CompactionEffectivenessTracker, CompactionReplayOutcome, try_compact_for_retry_checked,
};
use serde_json::{Value, json};

#[test]
fn a_changed_request_can_compact_after_a_futile_request() {
    let mut tracker = CompactionEffectivenessTracker::default();
    let mut protected = vec![
        json!({"role":"system","content":"required"}),
        json!({"role":"user","content":"current goal"}),
    ];
    let before = protected.clone();
    for _ in 0..3 {
        assert_eq!(
            try_compact_for_retry_checked(&mut protected, &mut tracker, Some(200_000), 100_000, 1),
            CompactionReplayOutcome::Futile
        );
        assert_eq!(protected, before);
    }
    assert_eq!(tracker.to_json()["consecutive_futile_attempts"], 3);
    let mut changed: Vec<Value> = vec![json!({"role":"system","content":"required"})];
    for i in 0..20 {
        changed.push(json!({"role":"user","content":format!("q{i}")}));
        changed.push(json!({"role":"assistant","content":"old detail ".repeat(2_000)}));
    }
    let latest = json!({"role":"user","content":"Preserve my current goal."});
    changed.push(latest.clone());
    let result =
        try_compact_for_retry_checked(&mut changed, &mut tracker, Some(200_000), 100_000, 1);
    assert!(
        matches!(result, CompactionReplayOutcome::Compacted(_)),
        "{result:?}"
    );
    assert!(changed.contains(&latest));
    assert_eq!(tracker.consecutive_futile_attempts, 0);
}
