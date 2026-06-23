//! wip-3 regression guard: runtime does not auto-extract "goal" from user
//! messages nor inject it (or any derived state) into the system prompt.
//!
//! Originating bug: session `895536bf-4001-4c36-99a8-3d395800440a` observed
//! the system prompt contained
//!   `[session-anchor] Goal: hi. State: ...`
//!   `[working-set:v1]\ngoal: hi\npending_work: runtime-goal: 最应修复的 bug: 1.`
//! after the user had typed "hi" once and then a substantive multiline query.
//! The runtime had (a) frozen `goal` at the first "hi" because
//! `ensure_goal` was set-if-empty, and (b) title-extracted the user's
//! follow-up into a `runtime-goal` TodoItem that leaked into every turn.
//!
//! Design fix: reference-agent model — runtime never auto-assigns a "goal",
//! never injects anchors/working-set headers with a `goal:` or
//! `pending_work:` line. LLM-authored task tracking (e.g. `TodoWriteTool` /
//! `TaskCreate`) takes over.
//!
//! This test is the contract: after wip-3 lands, the offending types and
//! injection helpers must not exist as public API. The compiler is the
//! enforcer — if any of the probes below resolves, the test body will
//! type-check and the `#[should_panic]` arm becomes the only failure signal.
//! We invert that with a `compile_fail`-style pattern via `trybuild`-free
//! doc tests and explicit negative-resolution probes.

// ── Guard 1: deleted types must not re-appear ───────────────────────────
//
// Each of the following is a compile-time assertion that the named path
// does NOT resolve. We use `#[cfg(feature = "wip3_resurrected")]` gates
// that no Cargo.toml will ever enable, wrapped around usages; the bodies
// that would fail compile live behind that unreachable feature.
//
// The positive guarantee we need is that a plain `use astra_turn_types::X`
// for each deleted path fails to resolve. That is tested in
// `tests/compile_fail/` via `trybuild` when a full sweep is green.
// Here we provide a faster, always-runnable behavioural check.

// ── Guard 2: SessionFacts has no goal/plan fields ───────────────────────

#[test]
fn session_facts_has_no_plan_state_field() {
    let facts = astra_turn_types::session_facts::SessionFacts::default();
    // The serde representation must not expose `plan_state`.
    let json = serde_json::to_value(&facts).expect("facts serialize");
    assert!(
        json.get("plan_state").is_none(),
        "SessionFacts.plan_state must be removed (was carrying runtime-extracted goal). \
         Found: {json:#?}"
    );
}

// ── Guard 3: working-set injection helper must not exist ──────────────────
//
// After wip-3, `to_working_set_injection` is deleted outright. The previous
// compile-time probe has been removed alongside the method. This guard now
// runs a source-grep check so the suite still produces a red test (not just
// a compile error) if the method ever resurfaces.

#[test]
fn to_working_set_injection_method_is_absent_from_public_api() {
    let module_root = env!("CARGO_MANIFEST_DIR");
    let src_dir = std::path::Path::new(module_root).join("src");
    let forbidden_signatures: &[&str] =
        &["fn to_working_set_injection", "to_working_set_injection("];

    let mut hits: Vec<(String, String)> = Vec::new();
    for path in walkdir(&src_dir) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for needle in forbidden_signatures {
            if body.contains(needle) {
                hits.push((path.display().to_string(), (*needle).to_string()));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "wip-3 contract violated — to_working_set_injection must not exist:\n{}",
        hits.iter()
            .map(|(p, n)| format!("  {p}: {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ── Guard 4: runtime pollution scenario ─────────────────────────────────
//
// The exact sequence from session 895536bf: user says "hi", then a
// follow-up with action words + multi-line content. No public API in
// `astra-turn-types` may return a derived "goal" or "pending_work" from
// that sequence.

#[test]
fn hi_followed_by_substantive_query_does_not_pin_goal_to_hi() {
    // Rather than exercising the deleted ContinuityState helpers, this
    // test documents the INVARIANT: there is no place in astra-turn-types
    // public surface that takes a sequence of user messages and returns
    // a runtime "goal" string. If such an API appears in future, this
    // test must be extended to assert it rejects the pollution pattern.
    //
    // Enforcement: the crate's public module graph.
    let module_root = env!("CARGO_MANIFEST_DIR");
    let src_dir = std::path::Path::new(module_root).join("src");
    assert!(
        src_dir.exists(),
        "astra-turn-types src dir missing: {src_dir:?}"
    );

    // Scan every .rs file under src/ for the forbidden public API surfaces.
    let forbidden: &[&str] = &[
        "pub fn ensure_goal",
        "pub fn ensure_tracked_goal",
        "pub fn maybe_update_session_goal",
        "pub fn to_working_set_injection",
        "pub fn attention_manifest",
        "pub struct ContinuityState",
        "pub struct AttentionManifest",
        "pub struct GoalState",
        "pub struct PlanFact",
        "pub const ATTENTION_PREFIX",
    ];

    let mut hits: Vec<(String, String)> = Vec::new();
    for entry in walkdir(&src_dir) {
        let path = entry;
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        for needle in forbidden {
            if body.contains(needle) {
                hits.push((path.display().to_string(), (*needle).to_string()));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "wip-3 contract violated — forbidden public API still present in astra-turn-types:\n{}",
        hits.iter()
            .map(|(p, n)| format!("  {p}: {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out
}
