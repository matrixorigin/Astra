//! Anti-pattern lints for the concurrency / error-context audit.
//!
//! These tests pin the contracts established by the audit so future refactors
//! cannot silently regress to the broken patterns:
//!
//! * Lock-ordering / atomic-ordering bugs (concurrency cluster: C1, C2, C4–C6)
//! * Swallowed errors via `eprintln!` / bare `Err(_) => return` /
//!   `unwrap_or_default()` on JSON parse (error-context cluster: E3–E10)
//!
//! The tests deliberately use simple `include_str!` + substring scanning
//! rather than ripgrep so they run in any environment.

use std::path::Path;

fn read(path: &str) -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let p = Path::new(&manifest).join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

fn read_workspace(rel: &str) -> String {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let p = Path::new(&manifest)
        .parent()
        .expect("crates dir")
        .parent()
        .expect("rust dir")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

// ── C1: cancel_run must drop the runs write guard before persisting ─────────
#[test]
fn c1_cancel_run_drops_write_guard_before_persist() {
    let src = read("src/server/run_lifecycle.rs");
    let idx = src
        .find("async fn cancel_run(")
        .expect("cancel_run present");
    let end = (idx + 6000).min(src.len());
    let body = &src[idx..end];
    let drop_pos = body
        .find("drop(runs);")
        .expect("cancel_run must call drop(runs); explicitly to release the write lock");
    let persist_pos = body
        .find(".persist_status(&run_id, STATUS_CANCELLED")
        .expect("cancel_run must persist cancellation status");
    assert!(
        drop_pos < persist_pos,
        "cancel_run must `drop(runs)` BEFORE awaiting engine.persist_status; \
         otherwise the RwLock write guard is held across DB I/O and blocks all \
         readers (mirror the pause_run pattern at line ~2683)."
    );
}

// ── C2: cancel/pause flag loads must use Acquire (not Relaxed) ──────────────
#[test]
fn c2_cancel_pause_flag_loads_use_acquire() {
    let files = [
        "src/turn/agentic_loop_lifecycle.rs",
        "src/server/run_lifecycle.rs",
        "src/server/delegation_engine.rs",
        "src/turn/llm_client.rs",
    ];
    for f in files {
        let src = read(f);
        for (lineno, line) in src.lines().enumerate() {
            let l = line.trim();
            if !l.contains(".load(") {
                continue;
            }
            let touches_flag = l.contains("cancel_flag")
                || l.contains("pause_flag")
                || l.contains("cancellation.flag")
                || (l.contains("f.load(")
                    && (1..=3).any(|back| {
                        src.lines()
                            .nth(lineno.saturating_sub(back))
                            .unwrap_or("")
                            .contains("LlmCancel")
                    }));
            if touches_flag {
                assert!(
                    !l.contains("Ordering::Relaxed"),
                    "{f}:{} cancel/pause flag load must use Acquire, not Relaxed:\n  {l}",
                    lineno + 1
                );
            }
        }
    }
}

// ── C4: take_emitted_events must handle broadcast Lagged explicitly ─────────
#[test]
fn c4_take_emitted_events_handles_lagged() {
    let src = read("src/server/server_loop_host.rs");
    let idx = src
        .find("pub fn take_emitted_events")
        .expect("take_emitted_events present");
    let snippet = &src[idx..idx + 2000.min(src.len() - idx)];
    assert!(
        snippet.contains("TryRecvError::Lagged"),
        "take_emitted_events must explicitly handle TryRecvError::Lagged so progress \
         events after the gap are not silently dropped"
    );
    assert!(
        snippet.contains("TryRecvError::Empty"),
        "take_emitted_events must terminate on TryRecvError::Empty"
    );
    assert!(
        !snippet.contains("while let Ok(evt) = rx.try_recv()"),
        "take_emitted_events must not use the simple while-let pattern that silently \
         drops every event after a Lagged"
    );
}

// ── C4: ProgressBroadcaster default capacity must be at least 1024 ──────────
#[test]
fn c4_progress_broadcaster_default_capacity_bumped() {
    let src = read_workspace("crates/astra-turn-core/src/orchestration_progress.rs");
    let idx = src
        .find("impl Default for ProgressBroadcaster")
        .expect("Default impl present");
    let snippet = &src[idx..idx + 400.min(src.len() - idx)];
    assert!(
        snippet.contains("Self::new(1024)") || snippet.contains("Self::new(2048)"),
        "ProgressBroadcaster default capacity must be ≥1024 to avoid silent \
         broadcast overflow during high-fanout multi-agent runs:\n{snippet}"
    );
    assert!(
        !snippet.contains("Self::new(256)"),
        "ProgressBroadcaster default 256 reverted; bump to 1024+"
    );
}

// ── C5: record_sub_run must take both maps in one scope (atomic insert) ─────
#[test]
fn c5_record_sub_run_atomic_dual_lock() {
    let src = read("src/server/delegation_engine.rs");
    let idx = src
        .find("pub async fn record_sub_run")
        .expect("record_sub_run present");
    let end = idx + src[idx..].find("\n    }").expect("function end") + 6;
    let body = &src[idx..end];
    assert!(
        body.contains("LOCK ORDER"),
        "record_sub_run must document its lock order"
    );
    // The old anti-pattern took the second lock in a separate
    // `self.parents.write().await.insert(...)` chained statement after
    // releasing the first guard. Forbid that exact shape.
    assert!(
        !body.contains("self.parents.write().await.insert("),
        "record_sub_run must not take parents.write() in a chained statement \
         after delegations.write() releases — keep both guards live in a \
         single scope so concurrent is_sub_run() observes a consistent state"
    );
}

// ── C6: adjust_config dual-lock ordering must be documented ─────────────────
#[test]
fn c6_adjust_config_lock_order_documented() {
    let src = read("src/server/server_tool_executor.rs");
    let idx = src
        .find("self_mod_mutation_counter.lock()")
        .expect("counter lock present");
    let start = idx.saturating_sub(2048);
    let region = &src[start..idx];
    assert!(
        region.contains("LOCK ORDER")
            && region.contains("observability_session")
            && region.contains("self_mod_mutation_counter"),
        "adjust_config dual-lock site must carry a `LOCK ORDER: \
         observability_session → self_mod_mutation_counter` comment"
    );
}

// ── E3: write_heavy_step_checkpoint must NOT mutate state on disk failure ──
#[test]
fn e3_checkpoint_state_only_set_on_success() {
    let src = read("src/turn/agentic_loop_finalization.rs");
    let idx = src
        .find("pub(crate) fn try_write_heavy_checkpoint")
        .expect("try_write_heavy_checkpoint present");
    let end = idx + src[idx..].find("\n}\n").expect("end") + 2;
    let body = &src[idx..end];
    let set_pos = body
        .find("state.last_composite_snapshot = Some(snapshot);")
        .expect("state.last_composite_snapshot assignment present");
    let preceding = &body[..set_pos];
    let returns_in_failure = preceding.matches("return;").count();
    assert!(
        returns_in_failure >= 2,
        "try_write_heavy_checkpoint must `return` on each disk-write failure \
         (write_step_checkpoint AND write_composite_snapshot_index) so it never \
         leaves `state.last_composite_snapshot = Some(...)` pointing at a \
         non-existent file. Current body has only {returns_in_failure} early returns."
    );
}

// ── E4: models.rs must log + use parse_json_column (no silent JSON defaults)
#[test]
fn e4_models_json_columns_log_on_malformed() {
    let src = read_workspace("crates/services/src/models.rs");
    assert!(
        src.contains("fn parse_json_column"),
        "services::models must define a `parse_json_column` helper that logs \
         malformed payloads via tracing::error! before falling back to the default"
    );
    assert!(
        src.contains("\"malformed JSON column, using default\""),
        "parse_json_column helper must emit the canonical \"malformed JSON column\" \
         error message (target astra_services::models) so observability dashboards \
         can detect column corruption"
    );
    // The model_record_from_row body must not contain the silent
    // serde_json::from_str(..).unwrap_or_default() anti-pattern any more.
    let idx = src
        .find("fn model_record_from_row")
        .expect("model_record_from_row present");
    let end = idx + src[idx..].find("\n    }").expect("end") + 6;
    let body = &src[idx..end];
    assert!(
        !body.contains("serde_json::from_str(&supported_json).unwrap_or_default()"),
        "model_record_from_row must not silently default malformed JSON columns; \
         use parse_json_column(...) instead"
    );
}

// ── E5/E10/E7: forbid eprintln! in audited error paths ──────────────────────
#[test]
fn e5_e7_e10_eprintln_forbidden_in_audited_paths() {
    // E5 — session_restore::extract_session_state_from_metadata
    let src = read_workspace("crates/services/src/session_restore.rs");
    let idx = src
        .find("fn extract_session_state_from_metadata")
        .expect("extract_session_state_from_metadata present");
    let end = (idx + 4000).min(src.len());
    let body = &src[idx..end];
    assert!(
        !body.contains("eprintln!"),
        "extract_session_state_from_metadata must use tracing, not eprintln!"
    );
    assert!(
        body.contains("metadata JSON parse failed"),
        "extract_session_state_from_metadata must emit a tracing::error! with the \
         canonical \"metadata JSON parse failed\" message when serde_json fails"
    );

    // E7 — bridge_inprocess auxiliary routing event persist failure
    let src = read("src/turn/bridge_inprocess.rs");
    assert!(
        !src.contains("PERSIST_FAIL session="),
        "bridge_inprocess: legacy eprintln! \"PERSIST_FAIL session=\" must be \
         replaced with tracing::error!"
    );
    assert!(
        src.contains("auxiliary routing event persist failed"),
        "bridge_inprocess auxiliary routing event persist failure must emit the \
         canonical tracing::error! with target astra_runtime::bridge_inprocess"
    );

    // E10 — agentic_adaptive_tuning auto-tuning hot path
    let src = read("src/turn/agentic_adaptive_tuning.rs");
    assert!(
        !src.contains("eprintln!(\"[auto-tuning]"),
        "agentic_adaptive_tuning.rs must not use eprintln! for auto-tuning \
         feedback persistence — use tracing::warn! / tracing::info!"
    );
}

// ── E6: delegation engine journal init must log, not silently return ────────
#[test]
fn e6_delegation_journal_init_logs_on_failure() {
    let src = read("src/server/delegation_engine.rs");
    let idx = src
        .find("fn persist_journal_entry")
        .expect("persist_journal_entry present");
    let end = idx + src[idx..].find("\n    }").expect("end") + 6;
    let body = &src[idx..end];
    assert!(
        !body.contains("Err(_) => return"),
        "persist_journal_entry must log JournalWriter::new failure \
         (agent_warn!), not silently swallow with `Err(_) => return`"
    );
    assert!(
        body.contains("JournalWriter::new failed"),
        "persist_journal_entry must emit a warn including `JournalWriter::new failed`"
    );
}

// ── E8: team_persistence row_to_team_definition must propagate worktree errors
#[test]
fn e8_worktree_mode_parse_error_propagated() {
    let src = read_workspace("crates/services/src/team_persistence.rs");
    let idx = src
        .find("fn row_to_team_definition")
        .expect("row_to_team_definition present");
    let end = idx + src[idx..].find("\n}\n").expect("end") + 2;
    let body = &src[idx..end];
    let wt_idx = body
        .find("worktree_mode:")
        .or_else(|| body.find("worktree_mode ="))
        .expect("worktree_mode binding present");
    let wt_end = wt_idx + body[wt_idx..].find(";\n").expect("end of binding");
    let wt_line = &body[wt_idx..wt_end];
    assert!(
        !wt_line.contains(".unwrap_or_default()"),
        "row_to_team_definition: worktree_mode parse must not silently default — \
         use `.map_err(...)?` so callers see the corruption:\n{wt_line}"
    );
    assert!(
        body.contains("invalid worktree_mode"),
        "row_to_team_definition: worktree_mode parse error must include the \
         offending value in the propagated error message"
    );
}
