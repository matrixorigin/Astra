//! Session-end governance — cleanup + episode persistence + reflection.
//!
//! At session end we:
//!
//! 1. **Read the final canonical session snapshot** while it still exists.
//! 2. **Write an `episodic` memory** summarising goals, outcomes, corrections,
//!    learnings, and structured [`SessionFacts`] (deterministic, no LLM call).
//! 3. **Purge working memory** tied to the session only after the episode is
//!    durable or the write was attempted.
//! 4. **Trigger reflection** so Memoria's graph-consolidation picks up
//!    recent memories into scene nodes. Respects the backend's cooldown
//!    (v1 defaults to 1h), so hot sessions won't thrash.
//!
//! All three are best-effort — a failure in any step logs a warning and
//! moves on; the rest still runs.

use astra_turn_types::session_facts::SessionFacts;

use crate::session_memory::runner::SessionNarrative;

/// Build the episodic summary content for a finished session. Pure
/// function — deterministic, no LLM call. ~200-500 chars.
///
/// Shape (matches the category layout in `astra_prompts::memory_types`):
/// ```text
/// [episode] turn=N, ~Kt tokens
/// Files touched: <k1>, <k2>, ...
/// Tools: <n1>:ok, <n2>:fail, ...
/// Errors: <last_error>
/// ```
pub fn build_episode_overview(facts: &SessionFacts) -> Option<String> {
    build_episode_overview_with_narrative(facts, None)
}

/// Build a cross-session episode from the final working snapshot plus
/// deterministic runtime facts. Working state answers "where were we?";
/// episodes answer "what happened and what is worth reconstructing later?".
/// Keeping the conversion deterministic prevents a second summarizer from
/// inventing long-term facts at the most durable boundary.
pub fn build_episode_overview_with_narrative(
    facts: &SessionFacts,
    narrative: Option<&SessionNarrative>,
) -> Option<String> {
    // Skip trivial sessions (nothing happened worth remembering).
    if facts.turn == 0
        && facts.active_files.is_empty()
        && facts.recent_tool_calls.is_empty()
        && narrative.is_none_or(session_narrative_is_empty)
    {
        return None;
    }

    let mut s = String::with_capacity(512);
    if let Some(narrative) = narrative {
        let goal = if !narrative.task_spec.trim().is_empty() {
            narrative.task_spec.as_str()
        } else {
            narrative.session_title.as_str()
        };
        push_episode_scalar(&mut s, "Goal", goal, 320);
        push_episode_items(&mut s, "Outcome/state", &narrative.current_state, 3, 220);
        push_episode_items(&mut s, "Open loops", &narrative.pending_todos, 3, 180);
        push_episode_items(&mut s, "Corrections", &narrative.corrections, 3, 220);
        push_episode_items(&mut s, "Learnings", &narrative.learnings, 3, 220);
    }
    s.push_str(&format!(
        "Runtime: turn={}, ~{}K tokens\n",
        facts.turn,
        facts.estimated_tokens / 1000
    ));

    // Files (most recent last — keep final FILES_CAP for brevity).
    // When truncating, append `(+N more)` so the reader knows the list
    // is abbreviated — silent truncation makes the episode look
    // complete when it isn't.
    const FILES_CAP: usize = 8;
    if !facts.active_files.is_empty() {
        let total = facts.active_files.len();
        let recent: Vec<&str> = facts
            .active_files
            .iter()
            .rev()
            .take(FILES_CAP)
            .map(|f| f.path.as_str())
            .collect();
        s.push_str("Files: ");
        s.push_str(&recent.iter().rev().copied().collect::<Vec<_>>().join(", "));
        if total > FILES_CAP {
            s.push_str(&format!(" (+{} more)", total - FILES_CAP));
        }
        s.push('\n');
    }

    // Tools: a compact ok/fail tally by name.
    const TOOLS_CAP: usize = 6;
    if !facts.recent_tool_calls.is_empty() {
        use std::collections::BTreeMap;
        let mut by_name: BTreeMap<&str, (u32, u32)> = BTreeMap::new();
        for tc in &facts.recent_tool_calls {
            let e = by_name.entry(tc.name.as_str()).or_insert((0, 0));
            if tc.ok {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        let total = by_name.len();
        let parts: Vec<String> = by_name
            .iter()
            .take(TOOLS_CAP)
            .map(|(name, (ok, fail))| match (*ok, *fail) {
                (o, 0) => format!("{name}:{o}ok"),
                (0, f) => format!("{name}:{f}fail"),
                (o, f) => format!("{name}:{o}ok/{f}fail"),
            })
            .collect();
        if !parts.is_empty() {
            s.push_str("Tools: ");
            s.push_str(&parts.join(", "));
            if total > TOOLS_CAP {
                s.push_str(&format!(" (+{} more)", total - TOOLS_CAP));
            }
            s.push('\n');
        }
    }

    // Errors (last one is the most informative; cap at 120 chars).
    if facts.error_state.total_errors > 0
        && let Some(err) = &facts.error_state.last_error
    {
        let err_snip: String = err.chars().take(120).collect();
        s.push_str("Last error: ");
        s.push_str(&err_snip);
        s.push('\n');
    }

    if s.trim().is_empty() { None } else { Some(s) }
}

fn session_narrative_is_empty(narrative: &SessionNarrative) -> bool {
    narrative.session_title.trim().is_empty()
        && narrative.task_spec.trim().is_empty()
        && narrative.current_state.is_empty()
        && narrative.active_goals.is_empty()
        && narrative.pending_todos.is_empty()
        && narrative.corrections.is_empty()
        && narrative.learnings.is_empty()
}

fn push_episode_scalar(out: &mut String, label: &str, value: &str, char_cap: usize) {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        return;
    }
    let value = value.chars().take(char_cap).collect::<String>();
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&value);
    out.push('\n');
}

fn push_episode_items(
    out: &mut String,
    label: &str,
    items: &[String],
    item_cap: usize,
    char_cap: usize,
) {
    let items = items
        .iter()
        .filter_map(|item| {
            let item = item.split_whitespace().collect::<Vec<_>>().join(" ");
            (!item.is_empty()).then(|| item.chars().take(char_cap).collect::<String>())
        })
        .take(item_cap)
        .collect::<Vec<_>>();
    if items.is_empty() {
        return;
    }
    out.push_str(label);
    out.push_str(": ");
    out.push_str(&items.join("; "));
    out.push('\n');
}

/// Full session-end governance:
///
/// 1. Read the final working snapshot and persist a deterministic episode.
/// 2. Purge session-scoped working memory after consolidation input is read.
/// 3. Trigger Memoria reflection (cooldown-respecting).
pub async fn run_session_end_governance(
    facts: &SessionFacts,
    session_id: &str,
    client: &dyn super::memoria_compact::MemoriaClient,
) -> Result<SessionEndReport, String> {
    let mut report = SessionEndReport::default();

    // Read before purge: the canonical narrative is the most valuable
    // consolidation input and working-memory cleanup must not destroy it.
    let snapshot =
        crate::session_memory::runner::load_current_session_memory_snapshot(client, session_id)
            .await;

    // ── 1. Persist episodic summary ────────────────────────────────
    let mut safe_to_purge_working = true;
    if let Some(overview) =
        build_episode_overview_with_narrative(facts, snapshot.as_ref().map(|s| &s.narrative))
    {
        match client.store_episode(session_id, &overview).await {
            Ok(memory_id) if !memory_id.is_empty() => {
                report.episode_memory_id = Some(memory_id);
                report.episode_chars = overview.chars().count();
                eprintln!(
                    "[session-end] Stored episode ({} chars) for session {session_id}",
                    report.episode_chars
                );
            }
            Ok(_) => {
                // store succeeded but response didn't include a memory_id;
                // count it as a write but leave id blank.
                report.episode_chars = overview.chars().count();
            }
            Err(e) => {
                safe_to_purge_working = false;
                report.working_retained_due_to_episode_failure = true;
                eprintln!("[session-end] store_episode failed: {e}");
            }
        }
    }

    // ── 2. Purge working memory ────────────────────────────────────
    if safe_to_purge_working {
        match client.purge_working(session_id).await {
            Ok(n) => {
                report.working_purged = n;
                eprintln!("[session-end] Purged {n} working memories for session {session_id}");
            }
            Err(e) => {
                eprintln!("[session-end] Failed to purge working memory: {e}");
            }
        }
    } else {
        eprintln!(
            "[session-end] Retained working memory for session {session_id} because episode persistence failed"
        );
    }

    // ── 3. Reflect + forward-feed scene candidates ─────────────────
    //
    // Reflect in `candidates` mode returns a list of scene clusters the
    // backend has grouped but not synthesized into scene nodes (the v1
    // LLM path requires a backend-side LLM_API_KEY that often isn't
    // configured). We forward-feed those clusters as `astra:scene`-
    // tagged semantic memories ourselves so the next session's prewarm
    // surfaces them alongside episodes. Without this step `reflect`
    // runs, produces output, and the output is silently discarded.
    match client.reflect_session(session_id, false).await {
        Ok(summary) => {
            report.reflect_candidates = summary.candidates;
            report.reflect_synthesized = summary.synthesized;
            for cand in &summary.candidate_payloads {
                match client
                    .store_scene(session_id, &cand.signal, &cand.summary)
                    .await
                {
                    Ok(memory_id) if !memory_id.is_empty() => {
                        report.scenes_stored += 1;
                    }
                    Ok(_) => report.scenes_stored += 1,
                    Err(e) => {
                        eprintln!("[session-end] store_scene failed: {e}");
                    }
                }
            }
        }
        Err(e) => {
            // Cooldown rejection is expected under hot activity; log at
            // warn and keep going.
            eprintln!("[session-end] reflect skipped/failed: {e}");
        }
    }

    Ok(report)
}

/// Report from session-end governance.
#[derive(Debug, Clone, Default)]
pub struct SessionEndReport {
    pub working_purged: u64,
    /// True when an episode was worth writing but failed to persist, so the
    /// working snapshot was intentionally retained for a future retry.
    pub working_retained_due_to_episode_failure: bool,
    /// Memoria memory_id of the persisted episode, if any.
    pub episode_memory_id: Option<String>,
    /// Characters written to the episode content (0 = no episode stored).
    pub episode_chars: usize,
    /// Number of scene / cluster candidates produced by reflect.
    pub reflect_candidates: u64,
    /// Whether reflect synthesized new scene nodes (v2 only).
    pub reflect_synthesized: bool,
    /// Number of `astra:scene`-tagged semantic memories the client
    /// forward-fed from reflect candidates into the store for
    /// next-session prewarm.
    pub scenes_stored: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::session_facts::{ErrorFact, FileEntry, ToolFact};

    #[test]
    fn episode_overview_none_for_trivial_session() {
        let facts = SessionFacts::default();
        assert!(build_episode_overview(&facts).is_none());
    }

    #[test]
    fn episode_overview_captures_files_tools_errors() {
        let facts = SessionFacts {
            active_files: vec![
                FileEntry {
                    path: "src/main.rs".into(),
                    last_action: "read".into(),
                    turn: 1,
                },
                FileEntry {
                    path: "src/lib.rs".into(),
                    last_action: "write".into(),
                    turn: 2,
                },
            ],
            recent_tool_calls: vec![
                ToolFact {
                    name: "read_file".into(),
                    ok: true,
                    turn: 1,
                },
                ToolFact {
                    name: "read_file".into(),
                    ok: true,
                    turn: 2,
                },
                ToolFact {
                    name: "bash".into(),
                    ok: false,
                    turn: 3,
                },
            ],
            error_state: ErrorFact {
                total_errors: 1,
                last_error: Some("cargo build failed: missing dep".into()),
                last_error_turn: Some(3),
            },
            turn: 3,
            estimated_tokens: 12_000,
            blocked_tools: vec![],
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        assert!(overview.starts_with("Runtime:"));
        assert!(overview.contains("turn=3"));
        assert!(overview.contains("~12K tokens"));
        assert!(overview.contains("src/main.rs"));
        assert!(overview.contains("src/lib.rs"));
        assert!(overview.contains("read_file:2ok"));
        assert!(overview.contains("bash:1fail"));
        assert!(overview.contains("cargo build failed"));
    }

    #[test]
    fn episode_consolidates_resumable_narrative_instead_of_only_runtime_telemetry() {
        let facts = SessionFacts {
            turn: 7,
            estimated_tokens: 8_000,
            ..Default::default()
        };
        let narrative = SessionNarrative {
            session_title: "Memory lifecycle review".into(),
            task_spec: "Make session and long-term memory form one typed lifecycle".into(),
            current_state: vec!["Typed episode storage is implemented".into()],
            active_goals: vec!["Complete lifecycle validation".into()],
            pending_todos: vec!["Run deployment-path behavior tests".into()],
            corrections: vec!["Do not treat policy evidence as a command".into()],
            learnings: vec!["Consolidation must read working state before purge".into()],
        };

        let overview = build_episode_overview_with_narrative(&facts, Some(&narrative))
            .expect("narrative episode");

        assert!(overview.starts_with("Goal: Make session and long-term memory"));
        assert!(overview.contains("Outcome/state: Typed episode storage is implemented"));
        assert!(overview.contains("Open loops: Run deployment-path behavior tests"));
        assert!(overview.contains("Corrections: Do not treat policy evidence as a command"));
        assert!(overview.contains("Learnings: Consolidation must read working state before purge"));
        assert!(overview.contains("Runtime: turn=7"));
    }

    #[test]
    fn episode_overview_marks_files_overflow_with_plus_n_more() {
        // 12 files exceeds the 8-file cap → "(+4 more)" annotation.
        let files: Vec<FileEntry> = (0..12)
            .map(|i| FileEntry {
                path: format!("src/file_{i}.rs"),
                last_action: "read".into(),
                turn: i,
            })
            .collect();
        let facts = SessionFacts {
            active_files: files,
            turn: 12,
            estimated_tokens: 5_000,
            ..Default::default()
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        assert!(overview.contains("(+4 more)"), "got: {overview}");
    }

    #[test]
    fn episode_overview_files_under_cap_has_no_plus_n_more() {
        let files: Vec<FileEntry> = (0..3)
            .map(|i| FileEntry {
                path: format!("src/file_{i}.rs"),
                last_action: "read".into(),
                turn: i,
            })
            .collect();
        let facts = SessionFacts {
            active_files: files,
            turn: 3,
            estimated_tokens: 1_000,
            ..Default::default()
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        assert!(!overview.contains("+ more"));
        assert!(!overview.contains("(+"));
    }

    #[test]
    fn episode_overview_marks_tools_overflow_with_plus_n_more() {
        // 9 distinct tool names exceeds the 6-tool cap → "(+3 more)".
        let tools: Vec<ToolFact> = (0..9)
            .map(|i| ToolFact {
                name: format!("tool_{i}"),
                ok: true,
                turn: i,
            })
            .collect();
        let facts = SessionFacts {
            recent_tool_calls: tools,
            turn: 9,
            estimated_tokens: 2_000,
            ..Default::default()
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        let tools_line = overview
            .lines()
            .find(|l| l.starts_with("Tools:"))
            .expect("tools line");
        assert!(
            tools_line.contains("(+3 more)"),
            "tools line missing overflow marker: {tools_line}"
        );
    }

    #[test]
    fn episode_overview_truncates_long_error() {
        let mut facts = SessionFacts {
            turn: 1,
            estimated_tokens: 500,
            ..Default::default()
        };
        facts.error_state = ErrorFact {
            total_errors: 1,
            last_error: Some("x".repeat(500)),
            last_error_turn: Some(1),
        };
        let overview = build_episode_overview(&facts).expect("non-trivial session");
        // 120-char cap on error snippet.
        assert!(!overview.contains(&"x".repeat(121)));
    }

    // ── P7: reflect candidates forward-fed as scene memories ───────────

    use super::super::memoria_compact::{
        MemoriaClient, MemoriaMemory, ReflectCandidate, ReflectSummary,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct SceneCaptureClient {
        scenes: Mutex<Vec<(String, String, String)>>, // session, signal, summary
        episodes: Mutex<Vec<String>>,
        reflect_response: Mutex<ReflectSummary>,
    }

    #[async_trait::async_trait]
    impl MemoriaClient for SceneCaptureClient {
        async fn retrieve_ext(
            &self,
            _q: &str,
            _sid: Option<&str>,
            _k: usize,
            _fs: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(Vec::new())
        }
        async fn store(
            &self,
            _c: &str,
            _t: &str,
            _s: Option<&str>,
            _tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("m".into())
        }
        async fn purge_working(&self, _s: &str) -> Result<u64, String> {
            Ok(0)
        }
        async fn store_episode(&self, sid: &str, overview: &str) -> Result<String, String> {
            self.episodes
                .lock()
                .unwrap()
                .push(format!("{sid}:{overview}"));
            Ok("ep_1".into())
        }
        async fn reflect_session(
            &self,
            _sid: &str,
            _force: bool,
        ) -> Result<ReflectSummary, String> {
            Ok(self.reflect_response.lock().unwrap().clone())
        }
        async fn store_scene(
            &self,
            sid: &str,
            signal: &str,
            summary: &str,
        ) -> Result<String, String> {
            self.scenes.lock().unwrap().push((
                sid.to_string(),
                signal.to_string(),
                summary.to_string(),
            ));
            Ok("scene_1".into())
        }
    }

    #[tokio::test]
    async fn governance_forward_feeds_reflect_candidates_as_scenes() {
        let client = Arc::new(SceneCaptureClient::default());
        *client.reflect_response.lock().unwrap() = ReflectSummary {
            synthesized: false,
            candidates: 2,
            candidate_payloads: vec![
                ReflectCandidate {
                    signal: "auth".into(),
                    importance: 0.8,
                    summary: "- fixed OAuth redirect\n- added MFA".into(),
                },
                ReflectCandidate {
                    signal: "tests".into(),
                    importance: 0.5,
                    summary: "- flaky integration suite".into(),
                },
            ],
            diagnostics: String::new(),
        };
        let facts = SessionFacts {
            turn: 2,
            estimated_tokens: 1_000,
            ..Default::default()
        };
        let report = run_session_end_governance(&facts, "sess-p7", client.as_ref())
            .await
            .expect("governance ok");
        assert_eq!(report.scenes_stored, 2);
        let scenes = client.scenes.lock().unwrap().clone();
        assert_eq!(scenes.len(), 2);
        assert_eq!(scenes[0].0, "sess-p7");
        assert_eq!(scenes[0].1, "auth");
        assert!(scenes[0].2.contains("OAuth redirect"));
        assert_eq!(scenes[1].1, "tests");
    }

    #[tokio::test]
    async fn governance_skips_scene_store_when_reflect_has_no_candidates() {
        let client = Arc::new(SceneCaptureClient::default());
        // Default = empty candidate_payloads
        let facts = SessionFacts {
            turn: 2,
            estimated_tokens: 500,
            ..Default::default()
        };
        let report = run_session_end_governance(&facts, "sess-p7-empty", client.as_ref())
            .await
            .expect("governance ok");
        assert_eq!(report.scenes_stored, 0);
        assert!(client.scenes.lock().unwrap().is_empty());
    }

    #[derive(Default)]
    struct LifecycleOrderClient {
        operations: Mutex<Vec<String>>,
        episode: Mutex<Option<String>>,
        fail_episode: bool,
    }

    #[async_trait::async_trait]
    impl super::super::memoria_compact::MemoriaClient for LifecycleOrderClient {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<super::super::memoria_compact::MemoriaMemory>, String> {
            self.operations.lock().unwrap().push("retrieve".into());
            Ok(vec![super::super::memoria_compact::MemoriaMemory {
                memory_id: "working-final".into(),
                content: crate::session_memory::runner::encode_session_memory_entry(
                    "session-lifecycle",
                    "# Session Memory\n\n## Task Specification\nBuild durable memory\n\n## Current State\n- Typed lifecycle complete\n\n## Learnings\n- Read working state before purge",
                ),
                memory_type: "working".into(),
                session_id: Some("session-lifecycle".into()),
                ..Default::default()
            }])
        }

        async fn store(
            &self,
            _content: &str,
            _memory_type: &str,
            _session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            Ok("unused".into())
        }

        async fn store_episode(&self, _session_id: &str, overview: &str) -> Result<String, String> {
            self.operations.lock().unwrap().push("store_episode".into());
            *self.episode.lock().unwrap() = Some(overview.to_string());
            if self.fail_episode {
                Err("episode store unavailable".into())
            } else {
                Ok("episode-1".into())
            }
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            self.operations.lock().unwrap().push("purge".into());
            Ok(1)
        }

        async fn reflect_session(
            &self,
            _session_id: &str,
            _force: bool,
        ) -> Result<super::super::memoria_compact::ReflectSummary, String> {
            self.operations.lock().unwrap().push("reflect".into());
            Ok(Default::default())
        }
    }

    #[tokio::test]
    async fn governance_consolidates_final_narrative_before_purging_working_state() {
        let client = LifecycleOrderClient::default();
        let facts = SessionFacts {
            turn: 9,
            estimated_tokens: 4_000,
            ..Default::default()
        };

        let report = run_session_end_governance(&facts, "session-lifecycle", &client)
            .await
            .expect("governance");

        assert_eq!(
            client.operations.lock().unwrap().as_slice(),
            ["retrieve", "store_episode", "purge", "reflect"]
        );
        let episode = client.episode.lock().unwrap().clone().expect("episode");
        assert!(episode.contains("Goal: Build durable memory"));
        assert!(episode.contains("Outcome/state: Typed lifecycle complete"));
        assert!(episode.contains("Learnings: Read working state before purge"));
        assert_eq!(report.working_purged, 1);
        assert_eq!(report.episode_memory_id.as_deref(), Some("episode-1"));
    }

    #[tokio::test]
    async fn governance_retains_working_snapshot_when_episode_persistence_fails() {
        let client = LifecycleOrderClient {
            fail_episode: true,
            ..Default::default()
        };
        let facts = SessionFacts {
            turn: 3,
            ..Default::default()
        };

        let report = run_session_end_governance(&facts, "session-lifecycle", &client)
            .await
            .expect("best-effort governance");

        assert_eq!(
            client.operations.lock().unwrap().as_slice(),
            ["retrieve", "store_episode", "reflect"]
        );
        assert_eq!(report.working_purged, 0);
        assert!(report.working_retained_due_to_episode_failure);
    }
}
