//! Session finalization.
//!
//! This module handles cleanup tasks when a REPL session ends:
//! - Writing session end journal events
//! - Finalizing workspace state
//! - Ending observability sessions
//! - Triggering Memoria governance and consolidation
//! - Clearing panic guards
//!
//! Lessons are extracted from the L1b narrative and tool signals, then
//! stored in Memoria as L3 durable memory (Session Memory Protocol §6.2).

use astra_services::session_artifact_store::SessionArtifactStore;
use astra_services::session_journal;
use std::time::Duration;

use super::ReplState;
use super::edge_tools;
use super::repl_turn::enqueue_ingestion_pub;
use super::session_guard::clear_panic_guard;

/// Finalize a REPL session: journal end event, persist state, extract learnings.
pub(super) async fn finalize_session(state: &mut ReplState) {
    // 1. Journal: session end event (idempotent — panic hook may have already written it)
    if let Some(ref j) = state.journal {
        let wrote =
            super::session_guard::try_write_session_end(j, state.session_id.as_deref(), state.turn);
        if wrote {
            let end_event =
                session_journal::JournalEvent::session_end(state.session_id.as_deref(), state.turn);
            enqueue_ingestion_pub(state, &end_event);
        }
    }
    // 2. Finalize workspace: persist compact summary + mark completed
    if state.turn > 0 {
        if let Some(ref sid) = state.session_id {
            astra_services::session_workspace::finalize_workspace_on_end(sid);
        }
    }
    // 3. Trigger Memoria governance + consolidation (best-effort with timeout)
    let gov_handle = tokio::spawn(edge_tools::memoria::memoria_governance_fire_and_forget());
    let con_handle = tokio::spawn(edge_tools::memoria::memoria_consolidate_fire_and_forget());
    // 3c. L3 knowledge backflow (Session Memory Protocol §6.2).
    //     Three sources merged into one batch Memoria write:
    //     (a) L1b narrative Learnings + User Corrections → semantic T2/T3
    //     (b) Checkpointer final flush (tool failures, stalls) → semantic T3
    //     (c) Episodic session summary → episodic T3
    if state.turn > 0 {
        let narrative = state.session_id.as_deref().and_then(|sid| {
            let raw = astra_services::local_session_artifact_store()
                .session_path(sid, "session-memory.md")
                .ok()
                .and_then(|p| astra_runtime::read_session_memory_file(&p))
                .or_else(|| {
                    let cwd = std::env::current_dir().ok()?;
                    let path = astra_runtime::resolve_resume_session_memory_file(
                        sid,
                        Some(cwd.to_str()?),
                    )?;
                    astra_runtime::read_session_memory_file(&path)
                })?;
            astra_runtime::turn::cloud::session_memory_protocol::SessionMemory::parse(&raw)
        });

        // (a) L1b narrative extraction
        let mut all_lessons =
            astra_runtime::lesson_synthesizer::extract_learnings_for_backflow(narrative.as_ref());

        // (b) Final tool/stall lessons — extract directly from signals and
        //     promote to semantic T3 (mid-session copies were working T4).
        //     Quality gate applied to filter out generic template content.
        let summary = match state
            .observability_session
            .as_ref()
            .and_then(|arc| arc.read().ok())
        {
            Some(guard) => astra_runtime::lesson_extractor::summarise_from_runtime(
                &state.tool_health_entries,
                Some(&*guard),
            ),
            None => astra_runtime::lesson_extractor::summarise_from_runtime(
                &state.tool_health_entries,
                None,
            ),
        };
        let signal_lessons = astra_runtime::lesson_extractor::extract_lessons(
            &summary,
            state.ingestion_user_id.as_deref().unwrap_or("unknown"),
            "generic",
            None,
        );
        for cl in signal_lessons {
            // Template-generated lessons use the basic gate (hedging + length).
            // The template blocklist is only for LLM-synthesized content.
            if astra_runtime::lesson_synthesizer::is_synthesized_lesson_acceptable(&cl.action) {
                all_lessons.push(astra_runtime::lesson_synthesizer::ExtractedLesson {
                    memory_type: "semantic",
                    content: format!("💡 LESSON: {}", cl.action),
                    trust_tier: "T3",
                });
            }
        }

        // (c) Episodic summary
        if let Some(episodic) = astra_runtime::lesson_synthesizer::build_episodic_summary(
            state.session_id.as_deref().unwrap_or("unknown"),
            state.turn,
            narrative.as_ref(),
        ) {
            all_lessons.push(episodic);
        }

        if !all_lessons.is_empty() {
            // Store T3 semantic lessons FIRST, then purge T4 working copies.
            // Sequenced to prevent the purge from racing ahead and deleting
            // in-flight T3 writes that share the same topic prefix.
            let sid_for_purge = state.session_id.clone();
            tokio::spawn(async move {
                edge_tools::memoria::memoria_store_lessons_fire_and_forget(
                    all_lessons,
                    sid_for_purge.clone(),
                )
                .await;
                // Only purge AFTER store completes.
                if let Some(sid) = sid_for_purge {
                    if let Some((client, base, key)) =
                        edge_tools::memoria::memoria_oneshot_client_pub(5)
                    {
                        let _ = client
                            .post(format!("{base}/v1/memories/purge"))
                            .header("Authorization", format!("Bearer {key}"))
                            .json(&serde_json::json!({
                                "topic": format!("LESSON session:{sid}"),
                                "reason": "session-end promotion to semantic T3",
                            }))
                            .send()
                            .await;
                    }
                }
            });
        } else if let Some(ref sid) = state.session_id {
            // No new lessons but still purge stale T4 working copies.
            let sid = sid.clone();
            tokio::spawn(async move {
                if let Some((client, base, key)) =
                    edge_tools::memoria::memoria_oneshot_client_pub(5)
                {
                    let _ = client
                        .post(format!("{base}/v1/memories/purge"))
                        .header("Authorization", format!("Bearer {key}"))
                        .json(&serde_json::json!({
                            "topic": format!("LESSON session:{sid}"),
                            "reason": "session-end cleanup (no new lessons)",
                        }))
                        .send()
                        .await;
                }
            });
        }
    }
    // 3f. Drain any in-flight memory extraction (bounded 5s).
    if let Some(outcome) = state.memory_extractor.drain(Duration::from_secs(5)).await {
        if let Some(ref j) = state.journal {
            let (saved, cats, dur, pfx) = match &outcome {
                super::memory_extraction::ExtractionOutcome::Extracted {
                    count,
                    categories,
                    duration_ms,
                    prefix_reused,
                } => (*count, categories.clone(), *duration_ms, *prefix_reused),
                _ => (0, vec![], 0, false),
            };
            let evt = session_journal::JournalEvent::memory_extraction_ex(
                state.session_id.as_deref(),
                state.turn,
                outcome.tag(),
                saved,
                &cats,
                dur,
                pfx,
            );
            let _ = j.append(&evt);
            enqueue_ingestion_pub(state, &evt);
        }
    }
    // 3e. End observability only after session-derived lessons/outcomes have
    // been persisted so the lifecycle boundary matches the data flow.
    if let (Some(hub), Some(session_id)) = (&state.observability_hub, &state.session_id) {
        let _ = hub.end_session(session_id);
    }
    // 4. Graceful ingestion shutdown: await worker flush
    if let Some(mc) = state.matrix_runtime.as_ref() {
        mc.shutdown_ingestion_and_wait().await;
    }
    // 5. Await Memoria maintenance (bounded 5s so we don't hang on exit)
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = gov_handle.await;
        let _ = con_handle.await;
    })
    .await;
    // 6. Clear panic guard
    clear_panic_guard();
}
