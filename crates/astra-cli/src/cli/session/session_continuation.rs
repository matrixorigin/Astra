//! Session continuation helpers: load previous conversation from the durable
//! conversation projection or journal, then strip runtime-injected scaffolding.
//!
//! Used by one-shot mode (`-m "..." --session-id <id>`) to provide multi-turn continuity.

use serde_json::Value;

#[derive(Debug)]
pub(crate) struct SessionContinuation {
    pub(crate) completed_turn_count: Option<u32>,
    pub(crate) messages: Vec<Value>,
    pub(crate) activated_deferred_tool_names: Vec<String>,
    pub(crate) active_conversation: astra_turn_core::active_conversation::ActiveConversation,
    pub(crate) resume: astra_turn_types::ResumeDescriptorV1,
}

fn resume_descriptor(
    source: astra_turn_types::ResumeSourceV1,
    cursor: astra_turn_types::SessionCursorV1,
    degraded_reasons: Vec<astra_turn_types::ResumeDegradedReasonV1>,
    repair_actions: Vec<astra_turn_types::ResumeRepairActionV1>,
) -> astra_turn_types::ResumeDescriptorV1 {
    astra_turn_types::ResumeDescriptorV1 {
        source,
        cursor,
        degraded_reasons,
        repair_actions,
    }
}

fn canonical_cli_owner_id() -> String {
    crate::cli::cli_config::cli_utils::cli_user_id()
}

pub(crate) fn is_attached_cli_canonical_owner(owner_id: &str) -> bool {
    owner_id == canonical_cli_owner_id() || owner_id == astra_services::local_owner_scope().id()
}

pub(crate) fn portable_resume_cursor(
    mut cursor: astra_turn_types::SessionCursorV1,
) -> astra_turn_types::SessionCursorV1 {
    if cursor.owner_id == astra_services::local_owner_scope().id() {
        cursor.owner_id = canonical_cli_owner_id();
    }
    cursor
}

pub(crate) fn portable_resume_descriptor(
    mut descriptor: astra_turn_types::ResumeDescriptorV1,
) -> astra_turn_types::ResumeDescriptorV1 {
    descriptor.cursor = portable_resume_cursor(descriptor.cursor);
    descriptor
}

pub(crate) fn cursor_is_exact_for_attached_account(
    source: &astra_turn_types::SessionCursorV1,
    selected: &astra_turn_types::SessionCursorV1,
) -> bool {
    is_attached_cli_canonical_owner(&source.owner_id)
        && astra_turn_types::cursor_relation(
            &portable_resume_cursor(source.clone()),
            &portable_resume_cursor(selected.clone()),
        ) == astra_turn_types::CursorRelationV1::Exact
}

fn select_single_resume_descriptor(
    canonical_head: Option<&astra_turn_types::SessionCursorV1>,
    descriptor: astra_turn_types::ResumeDescriptorV1,
    messages: &[Value],
) -> Option<astra_turn_types::ResumeDescriptorV1> {
    if astra_turn_types::canonical_conversation_root(messages)
        != descriptor.cursor.canonical_root_hash
    {
        tracing::warn!(
            source = ?descriptor.source,
            "resume candidate payload does not materialize its declared canonical root"
        );
        return None;
    }
    astra_turn_types::select_resume_candidate_index(
        canonical_head,
        std::slice::from_ref(&descriptor),
    )
    .map_err(|error| {
        tracing::warn!(%error, "failed to select a causally consistent resume candidate");
        error
    })
    .ok()
    .map(|_| descriptor)
}

pub(crate) fn continuation_from_resume_bundle(
    bundle: astra_turn_types::ResumeBundleV1,
) -> Option<SessionContinuation> {
    if bundle.schema_version != astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION {
        tracing::warn!(
            schema_version = bundle.schema_version,
            "resume bundle schema is unsupported"
        );
        return None;
    }
    if !is_attached_cli_canonical_owner(&bundle.cursor.owner_id) {
        tracing::warn!(
            cursor_owner = %bundle.cursor.owner_id,
            "resume bundle does not belong to the active account"
        );
        return None;
    }
    if !bundle.validates_root() {
        tracing::warn!("resume bundle payload does not materialize its declared canonical root");
        return None;
    }
    let astra_turn_types::ResumeBundleV1 {
        schema_version: _,
        cursor,
        source: resume_source,
        conversation_messages: messages,
        materialized_conversation_root_hash: _,
        degraded_reasons,
        repair_actions,
        projections,
    } = bundle;
    let projection_activation = projections
        .activation_at(&cursor)
        .into_iter()
        .flat_map(|projection| projection.deferred_tool_names.iter().cloned());
    let activated_deferred_tool_names =
        continuation_activation_names(&messages, projection_activation);
    let source = match resume_source {
        astra_turn_types::ResumeSourceV1::CanonicalJournal => {
            astra_turn_core::active_conversation::ActiveConversationSource::Journal
        }
        astra_turn_types::ResumeSourceV1::CslProjection => {
            astra_turn_core::active_conversation::ActiveConversationSource::CslProjection
        }
        astra_turn_types::ResumeSourceV1::Checkpoint => {
            astra_turn_core::active_conversation::ActiveConversationSource::Checkpoint
        }
        astra_turn_types::ResumeSourceV1::JournalDisplayProjection
        | astra_turn_types::ResumeSourceV1::TranscriptProjection => {
            astra_turn_core::active_conversation::ActiveConversationSource::LegacyDisplayProjection
        }
    };
    // The server's schema-v2 cursor identifies the immutable manifest, while
    // the local ActiveConversation journal identifies its flattened
    // projection. Keep the authoritative cursor in `resume`, but derive a
    // schema-v1 content cursor for the local projection store.
    let mut projection_cursor = cursor.clone();
    if projection_cursor.projection_schema
        == astra_turn_types::SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION
    {
        projection_cursor.projection_schema =
            astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION;
        projection_cursor.canonical_root_hash =
            astra_turn_types::canonical_conversation_root(&messages);
    }
    let active_conversation =
        astra_turn_core::active_conversation::ActiveConversation::from_cursor_projection(
            projection_cursor,
            messages.clone(),
            source,
        )
        .ok()?;
    Some(SessionContinuation {
        completed_turn_count: Some(cursor.completed_turn),
        activated_deferred_tool_names,
        messages,
        active_conversation,
        resume: resume_descriptor(resume_source, cursor, degraded_reasons, repair_actions),
    })
}

pub(crate) fn materialize_cli_continuation_messages(
    site: astra_core::history_work::HistoryWorkSite,
    messages: &[Value],
) -> Vec<Value> {
    crate::cli::history_work::clone_json_history(site, messages)
}

fn record_session_restore_hydration(messages: &[Value]) {
    crate::cli::history_work::record_json_history(
        astra_core::history_work::HistoryWorkSite::CliSessionRestoreHydration,
        messages,
    );
}

pub(crate) fn continuation_activation_names(
    messages: &[Value],
    persisted_names: impl IntoIterator<Item = String>,
) -> Vec<String> {
    astra_turn_core::tool::deferred_activation::merged_activated_tool_names(
        messages,
        persisted_names,
    )
}

/// Load prompt-facing continuation from canonical local session state.
/// Used by one-shot mode (`-m "..." --session-id <id>`) to provide
/// conversation history that the model needs for multi-turn continuity.
///
/// Typed commits in the primary session journal are the durable canonical
/// source. CSL and heavy checkpoints are admitted only with an exact versioned
/// cursor. Display-oriented history is never used as model history.
pub(crate) fn load_session_messages_for_continuation(session_id: &str) -> Option<Vec<Value>> {
    load_session_continuation_for_recovery(session_id).map(|continuation| continuation.messages)
}

pub(crate) fn load_session_continuation_for_recovery(
    session_id: &str,
) -> Option<SessionContinuation> {
    match load_journal_canonical_conversation(session_id) {
        Ok(Some(active_conversation)) => {
            let messages = active_conversation.materialize();
            record_session_restore_hydration(&messages);
            let cursor = active_conversation.cursor().clone();
            let activated_deferred_tool_names = if cursor.conversation_seq == 0 {
                // The journal file itself is the canonical empty-state marker.
                // No side projection is allowed to populate either messages
                // or activation state before the first canonical commit.
                continuation_activation_names(&messages, Vec::new())
            } else {
                let checkpoint = load_heavy_checkpoint(session_id);
                let checkpoint_activation = checkpoint
                    .as_ref()
                    .filter(|checkpoint| {
                        checkpoint
                            .conversation_cursor
                            .as_ref()
                            .is_some_and(|checkpoint_cursor| {
                                astra_turn_types::cursor_relation(checkpoint_cursor, &cursor)
                                    == astra_turn_types::CursorRelationV1::Exact
                            })
                    })
                    .into_iter()
                    .flat_map(|checkpoint| {
                        checkpoint.activated_deferred_tool_names.iter().cloned()
                    });
                let csl_activation = load_csl_continuation(session_id)
                    .ok()
                    .flatten()
                    .filter(|continuation| {
                        astra_turn_types::cursor_relation(
                            continuation.active_conversation.cursor(),
                            &cursor,
                        ) == astra_turn_types::CursorRelationV1::Exact
                    })
                    .into_iter()
                    .flat_map(|continuation| continuation.activated_deferred_tool_names);
                continuation_activation_names(
                    &messages,
                    checkpoint_activation.chain(csl_activation),
                )
            };
            let resume = select_single_resume_descriptor(
                Some(&cursor),
                resume_descriptor(
                    astra_turn_types::ResumeSourceV1::CanonicalJournal,
                    cursor.clone(),
                    Vec::new(),
                    Vec::new(),
                ),
                &messages,
            )?;
            return Some(SessionContinuation {
                completed_turn_count: Some(active_conversation.cursor().completed_turn),
                activated_deferred_tool_names,
                messages,
                active_conversation,
                resume,
            });
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to replay canonical journal conversation; using an explicitly degraded recovery source"
            );
        }
    }

    match load_csl_continuation(session_id) {
        Ok(Some(continuation)) => return Some(continuation),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to read CSL continuation projection; falling back to durable journal"
            );
        }
    }

    let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    let heavy = load_heavy_checkpoint(session_id);

    match heavy {
        Some(cp) if !cp.messages.is_empty() => {
            record_session_restore_hydration(&cp.messages);
            let messages = match astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
                cp.messages,
            ) {
                    Ok(messages) => messages,
                    Err(error) => {
                        tracing::warn!(
                            user_id = %user_id,
                            session_id = %session_id,
                            error = %error,
                            "continuation checkpoint contains invalid typed turn metadata"
                        );
                        return None;
                    }
                };
            if messages.is_empty() {
                tracing::warn!(
                    user_id = %user_id,
                    session_id = %session_id,
                    "continuation checkpoint sanitized to no prompt-facing messages; falling back to transcript"
                );
                None
            } else {
                let cursor = cp.conversation_cursor.clone()?;
                if let Err(error) = validate_flat_projection_cursor(session_id, &cursor) {
                    tracing::warn!(
                        session_id,
                        %error,
                        "checkpoint cursor is not exact for the requested session"
                    );
                    return None;
                }
                let active_conversation =
                    astra_turn_core::active_conversation::ActiveConversation::from_cursor_projection(
                        cursor.clone(),
                        messages.clone(),
                        astra_turn_core::active_conversation::ActiveConversationSource::Checkpoint,
                    )
                    .ok()?;
                let activated_deferred_tool_names =
                    continuation_activation_names(&messages, cp.activated_deferred_tool_names);
                let resume = select_single_resume_descriptor(
                    None,
                    resume_descriptor(
                        astra_turn_types::ResumeSourceV1::Checkpoint,
                        cursor.clone(),
                        vec![astra_turn_types::ResumeDegradedReasonV1::CheckpointFallback],
                        vec![
                            astra_turn_types::ResumeRepairActionV1::InspectCanonicalJournal,
                            astra_turn_types::ResumeRepairActionV1::RebuildProjectionFromJournal,
                        ],
                    ),
                    &messages,
                )?;
                Some(SessionContinuation {
                    completed_turn_count: Some(cursor.completed_turn),
                    activated_deferred_tool_names,
                    active_conversation,
                    messages,
                    resume,
                })
            }
        }
        _ => None,
    }
}

/// Recover the durable canonical conversation for a live turn boundary.
///
/// A journal with no conversation commits is not the same as unavailable
/// canonical state: it is an explicitly empty canonical conversation. This
/// occurs after a first-turn failure or a foreground-to-background handoff.
/// Materializing that empty state lets the next turn proceed without ever
/// promoting display-oriented pair history into a prompt source.
pub(crate) fn recover_or_initialize_active_conversation(
    session_id: &str,
) -> Result<astra_turn_core::active_conversation::ActiveConversation, String> {
    match load_journal_canonical_conversation(session_id)? {
        Some(active) => Ok(active),
        None => astra_turn_core::active_conversation::ActiveConversation::empty(
            &crate::cli::cli_config::cli_utils::cli_user_id(),
            session_id,
        )
        .map_err(|error| error.to_string()),
    }
}

/// Replay canonical prompt continuation from the same physical append-order
/// snapshot used to refresh the interactive turn counters. The caller must
/// hold the matching execution lease while obtaining and consuming `events`.
pub(crate) fn recover_or_initialize_active_conversation_from_append_order_events(
    session_id: &str,
    events: &[astra_services::session_journal::JournalEvent],
) -> Result<astra_turn_core::active_conversation::ActiveConversation, String> {
    active_conversation_from_append_order_events(session_id, events)?.map_or_else(
        || {
            astra_turn_core::active_conversation::ActiveConversation::empty(
                &crate::cli::cli_config::cli_utils::cli_user_id(),
                session_id,
            )
            .map_err(|error| error.to_string())
        },
        Ok,
    )
}

fn load_journal_canonical_conversation(
    session_id: &str,
) -> Result<Option<astra_turn_core::active_conversation::ActiveConversation>, String> {
    let events = astra_services::session_journal::read_journal_append_order(session_id)
        .map_err(|error| error.to_string())?;
    let journal_exists = astra_services::session_journal::journal_file_path(session_id).is_file();
    if !journal_exists {
        return Ok(None);
    }
    active_conversation_from_append_order_events(session_id, &events)
}

fn active_conversation_from_append_order_events(
    session_id: &str,
    events: &[astra_services::session_journal::JournalEvent],
) -> Result<Option<astra_turn_core::active_conversation::ActiveConversation>, String> {
    let commits = events
        .iter()
        .filter_map(|event| event.conversation_commit.clone())
        .collect::<Vec<_>>();
    let owner_id = commits
        .first()
        .map(|commit| commit.cursor.owner_id.clone())
        .unwrap_or_else(|| astra_services::local_owner_scope().id().to_string());
    if !is_attached_cli_canonical_owner(&owner_id) {
        return Err(format!(
            "canonical journal belongs to owner `{owner_id}`, not the attached account"
        ));
    }
    if commits.is_empty() {
        return astra_turn_core::active_conversation::ActiveConversation::from_projection(
            &owner_id,
            session_id,
            Vec::new(),
            0,
            astra_turn_core::active_conversation::ActiveConversationSource::Journal,
        )
        .map(Some)
        .map_err(|error| error.to_string());
    }
    astra_turn_core::active_conversation::ActiveConversation::replay(&owner_id, session_id, commits)
        .map_err(|error| error.to_string())
}

fn validate_flat_projection_cursor(
    session_id: &str,
    cursor: &astra_turn_types::SessionCursorV1,
) -> Result<(), String> {
    if !is_attached_cli_canonical_owner(&cursor.owner_id) {
        return Err(format!(
            "cursor belongs to owner `{}`, not the attached account",
            cursor.owner_id
        ));
    }
    if cursor.session_id != session_id {
        return Err(format!(
            "cursor belongs to session `{}`, expected `{session_id}`",
            cursor.session_id
        ));
    }
    if cursor.branch_id != astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID {
        return Err(format!(
            "cursor branch `{}` is unsupported",
            cursor.branch_id
        ));
    }
    if cursor.schema_version != astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION {
        return Err(format!(
            "cursor schema {} is unsupported",
            cursor.schema_version
        ));
    }
    if cursor.projection_schema != astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION {
        return Err(format!(
            "cursor projection schema {} is unsupported for a flat projection",
            cursor.projection_schema
        ));
    }
    Ok(())
}

fn load_heavy_checkpoint(
    session_id: &str,
) -> Option<astra_pipeline::step_protocol::HeavyCheckpoint> {
    let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
    match astra_pipeline::step_checkpoint::read_latest_heavy_checkpoint(&user_id, session_id) {
        Ok(checkpoint) => checkpoint,
        Err(error) => {
            tracing::warn!(
                user_id = %user_id,
                session_id = %session_id,
                error = %error,
                "failed to read continuation checkpoint"
            );
            None
        }
    }
}

pub(crate) fn load_csl_continuation(
    session_id: &str,
) -> Result<Option<SessionContinuation>, String> {
    let store = astra_turn_core::conversation_log::file_store::FileCslStore::new(
        crate::cli::session::session_recovery::io::csl_store_base_dir(),
    );
    let materialized = store
        .load_materialized_blocking(session_id)
        .map_err(|error| error.to_string())?;
    let Some(materialized) = materialized else {
        return Ok(None);
    };
    let cursor = materialized
        .session_state
        .source_cursor
        .as_ref()
        .ok_or_else(|| format!("CSL continuation has no versioned cursor: {session_id}"))?;
    validate_flat_projection_cursor(session_id, cursor)?;
    let cursor = cursor.clone();
    record_session_restore_hydration(&materialized.messages);
    let messages = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(
        materialized.messages,
    )
    .map_err(|error| error.to_string())?;
    let activated_deferred_tool_names = continuation_activation_names(
        &messages,
        materialized.session_state.activated_deferred_tool_names,
    );
    let active_conversation =
        astra_turn_core::active_conversation::ActiveConversation::from_cursor_projection(
            cursor.clone(),
            messages.clone(),
            astra_turn_core::active_conversation::ActiveConversationSource::CslProjection,
        )
        .map_err(|error| error.to_string())?;
    let resume = select_single_resume_descriptor(
        None,
        resume_descriptor(
            astra_turn_types::ResumeSourceV1::CslProjection,
            cursor.clone(),
            Vec::new(),
            Vec::new(),
        ),
        &messages,
    )
    .ok_or_else(|| format!("failed to select CSL continuation for session {session_id}"))?;
    Ok((!messages.is_empty()).then_some(SessionContinuation {
        completed_turn_count: Some(cursor.completed_turn),
        active_conversation,
        messages,
        activated_deferred_tool_names,
        resume,
    }))
}

/// Strip runtime-injected scaffolding messages that must not persist across
/// turn boundaries. Without this, harness nudges (injected as "user" role)
/// bias the model toward tool usage on the next turn even when the user's
/// new message is purely conversational.
pub(crate) fn sanitize_continuation_messages(msgs: Vec<Value>) -> Vec<Value> {
    record_session_restore_hydration(&msgs);
    let (sanitized, invalid_turn_semantics_dropped) =
        astra_turn_core::prompt_facing::recover_canonical_continuation_messages_with_turn_semantics(
            msgs,
        );
    if invalid_turn_semantics_dropped > 0 {
        tracing::warn!(
            invalid_turn_semantics_dropped,
            "dropped invalid typed turn metadata while sanitizing continuation messages"
        );
    }
    sanitized
}

/// Extract text content from a message regardless of format.
/// Handles both string content and array-format content blocks.
pub(crate) fn extract_text_content(msg: &Value) -> Option<String> {
    astra_turn_core::prompt_facing::extract_text_content(msg)
}

/// Reconstruct CLI `(user, assistant)` history pairs from OpenAI-style messages.
///
/// Rules:
/// - ignore tool/system messages,
/// - ignore assistant tool-call stubs that have no visible text,
/// - concatenate multiple visible assistant chunks in the same turn.
pub(crate) fn history_pairs_from_messages(msgs: &[Value]) -> Vec<(String, String)> {
    let visible_msgs = astra_turn_core::prompt_facing::sanitize_user_visible_messages(
        materialize_cli_continuation_messages(
            astra_core::history_work::HistoryWorkSite::CliDisplayHistoryProjectionClone,
            msgs,
        ),
    );
    let mut pairs = Vec::new();
    let mut current_user = String::new();
    let mut current_assistant = String::new();

    for msg in &visible_msgs {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let text = extract_text_content(msg).unwrap_or_default();
        match role {
            "user" => {
                if !current_user.is_empty() || !current_assistant.is_empty() {
                    pairs.push((
                        std::mem::take(&mut current_user),
                        std::mem::take(&mut current_assistant),
                    ));
                }
                if !text.trim().is_empty() {
                    current_user = text;
                }
            }
            "assistant" => {
                if text.trim().is_empty() {
                    continue;
                }
                if current_user.is_empty() {
                    continue;
                }
                if current_assistant.is_empty() {
                    current_assistant = text;
                } else {
                    current_assistant.push_str("\n\n");
                    current_assistant.push_str(&text);
                }
            }
            _ => {}
        }
    }

    if !current_user.is_empty() || !current_assistant.is_empty() {
        pairs.push((current_user, current_assistant));
    }

    pairs
}
#[cfg(test)]
mod tests {
    use astra_pipeline::step_protocol::{ExecutionCursor, StepCheckpoint};
    use astra_services::session_journal;
    use serde_json::{Value, json};

    fn exact_test_cursor(
        session_id: &str,
        completed_turn: u32,
        messages: &[Value],
    ) -> astra_turn_types::SessionCursorV1 {
        astra_turn_core::active_conversation::ActiveConversation::empty(
            astra_services::local_owner_scope().id(),
            session_id,
        )
        .unwrap()
        .prepare_commit(completed_turn, None, messages.to_vec())
        .unwrap()
        .next
        .cursor()
        .clone()
    }

    fn exact_csl_state(
        session_id: &str,
        completed_turn: u32,
        messages: &[Value],
    ) -> astra_turn_core::conversation_log::SessionStateCompact {
        astra_turn_core::conversation_log::SessionStateCompact {
            source_cursor: Some(exact_test_cursor(session_id, completed_turn, messages)),
            ..Default::default()
        }
    }

    #[test]
    #[serial_test::serial]
    fn only_the_attached_local_owner_alias_maps_to_the_bound_account() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "migration-profile",
            Some("account-1"),
        )
        .unwrap();
        let local_owner = astra_services::local_owner_scope().id().to_string();
        assert_ne!(local_owner, "account-1");
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: local_owner.clone(),
            session_id: "session-1".into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: "root".into(),
            projection_schema: astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };

        let descriptor = astra_turn_types::ResumeDescriptorV1 {
            source: astra_turn_types::ResumeSourceV1::CanonicalJournal,
            cursor,
            degraded_reasons: Vec::new(),
            repair_actions: Vec::new(),
        };

        assert!(super::is_attached_cli_canonical_owner(&local_owner));
        assert!(super::is_attached_cli_canonical_owner("account-1"));
        assert!(!super::is_attached_cli_canonical_owner("account-b"));
        assert_eq!(
            super::portable_resume_descriptor(descriptor)
                .cursor
                .owner_id,
            "account-1"
        );
    }

    #[test]
    fn continuation_materialization_preserves_nested_typed_messages() {
        let messages = vec![json!({
            "role": "user",
            "content": ["hi", {"text": "nested"}],
        })];

        assert_eq!(
            super::materialize_cli_continuation_messages(
                astra_core::history_work::HistoryWorkSite::CliDisplayHistoryProjectionClone,
                &messages,
            ),
            messages,
            "instrumentation must not project or reinterpret typed continuation data"
        );
    }

    #[test]
    #[serial_test::serial]
    fn segmented_resume_keeps_authority_cursor_and_derives_local_content_cursor() {
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "segmented-resume-profile",
            Some("account-1"),
        )
        .unwrap();
        let messages = vec![json!({"role": "user", "content": "resume"})];
        let materialized_root = astra_turn_types::canonical_conversation_root(&messages);
        let cursor = astra_turn_types::SessionCursorV1 {
            schema_version: astra_turn_types::SESSION_CURSOR_SCHEMA_VERSION,
            owner_id: "account-1".into(),
            session_id: "segmented-session".into(),
            branch_id: astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID.into(),
            completed_turn: 1,
            journal_event_seq: 1,
            conversation_seq: 1,
            canonical_root_hash: "a".repeat(64),
            projection_schema: astra_turn_types::SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION,
            compaction_generation: 0,
            config_version_id: None,
        };
        let continuation =
            super::continuation_from_resume_bundle(astra_turn_types::ResumeBundleV1 {
                schema_version: astra_turn_types::RESUME_BUNDLE_SCHEMA_VERSION,
                cursor: cursor.clone(),
                source: astra_turn_types::ResumeSourceV1::CanonicalJournal,
                conversation_messages: messages,
                materialized_conversation_root_hash: Some(materialized_root.clone()),
                degraded_reasons: Vec::new(),
                repair_actions: Vec::new(),
                projections: Default::default(),
            })
            .expect("segmented canonical resume must create a local continuation");

        assert_eq!(continuation.resume.cursor, cursor);
        assert_eq!(
            continuation.active_conversation.cursor().projection_schema,
            astra_turn_types::CONVERSATION_PROJECTION_SCHEMA_VERSION
        );
        assert_eq!(
            continuation
                .active_conversation
                .cursor()
                .canonical_root_hash,
            materialized_root
        );
    }

    #[test]
    #[serial_test::serial]
    fn journal_without_a_conversation_commit_is_explicitly_empty_canonical_state() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("journal-continuation-{}", uuid::Uuid::new_v4());
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        writer
            .append(&session_journal::JournalEvent::turn(
                Some(&session_id),
                1,
                Some("test-model"),
                "keep the result",
                "the result is durable",
                0,
                5,
                3,
                10,
            ))
            .unwrap();
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let mut checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![
            json!({"role": "user", "content": "stale checkpoint question"}),
            json!({"role": "assistant", "content": "stale checkpoint answer"}),
        ];
        heavy.conversation_cursor = Some(
            astra_turn_core::active_conversation::ActiveConversation::empty(
                astra_services::local_owner_scope().id(),
                &session_id,
            )
            .unwrap()
            .cursor()
            .clone(),
        );
        heavy.activated_deferred_tool_names = vec!["github".to_string()];
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            2,
            &checkpoint,
        )
        .unwrap();
        let stale_csl = vec![
            json!({"role": "user", "content": "stale CSL question"}),
            json!({"role": "assistant", "content": "stale CSL answer"}),
        ];
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &stale_csl,
            &exact_csl_state(&session_id, 1, &stale_csl),
        )
        .unwrap();

        let continuation = super::load_session_continuation_for_recovery(&session_id)
            .expect("the journal explicitly owns an empty canonical generation");
        assert!(
            continuation.messages.is_empty(),
            "CSL and checkpoint content must not resurrect history past an existing empty journal"
        );
        assert_eq!(
            continuation.active_conversation.source(),
            astra_turn_core::active_conversation::ActiveConversationSource::Journal
        );
        assert_eq!(
            continuation.activated_deferred_tool_names,
            Vec::<String>::new(),
            "side projections from another cursor must not activate tools on the empty generation"
        );
    }

    #[test]
    #[serial_test::serial]
    fn unversioned_checkpoint_cannot_create_a_resume_projection() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("unversioned-checkpoint-{}", uuid::Uuid::new_v4());
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let mut checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![
            json!({"role": "user", "content": "unversioned question"}),
            json!({"role": "assistant", "content": "unversioned answer"}),
        ];
        assert!(heavy.conversation_cursor.is_none());
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            1,
            &checkpoint,
        )
        .unwrap();

        assert!(
            super::load_session_continuation_for_recovery(&session_id).is_none(),
            "checkpoint messages without a versioned cursor must not create resume authority"
        );
    }

    #[test]
    fn flat_projection_cursor_requires_exact_owner_session_branch_and_schema() {
        let session_id = "exact-session";
        let messages = vec![json!({"role": "user", "content": "private"})];
        let cursor = exact_test_cursor(session_id, 1, &messages);
        super::validate_flat_projection_cursor(session_id, &cursor).unwrap();

        let mut cases = Vec::new();
        let mut foreign_owner = cursor.clone();
        foreign_owner.owner_id = "foreign-owner".into();
        cases.push(foreign_owner);
        let mut foreign_session = cursor.clone();
        foreign_session.session_id = "other-session".into();
        cases.push(foreign_session);
        let mut foreign_branch = cursor.clone();
        foreign_branch.branch_id = "other-branch".into();
        cases.push(foreign_branch);
        let mut unsupported_cursor = cursor.clone();
        unsupported_cursor.schema_version += 1;
        cases.push(unsupported_cursor);
        let mut unsupported_projection = cursor;
        unsupported_projection.projection_schema =
            astra_turn_types::SEGMENTED_CONVERSATION_PROJECTION_SCHEMA_VERSION;
        cases.push(unsupported_projection);

        for invalid in cases {
            assert!(
                super::validate_flat_projection_cursor(session_id, &invalid).is_err(),
                "invalid projection identity was admitted: {invalid:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn csl_cursor_for_another_session_is_rejected_at_the_restore_boundary() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("requested-csl-{}", uuid::Uuid::new_v4());
        let messages = vec![json!({"role": "user", "content": "private"})];
        let state = exact_csl_state("different-session", 1, &messages);
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &messages,
            &state,
        )
        .unwrap();

        let error = super::load_csl_continuation(&session_id)
            .expect_err("a CSL cursor cannot cross a session boundary");
        assert!(error.contains("different-session"), "{error}");
    }

    #[test]
    #[serial_test::serial]
    fn canonical_journal_recovers_typed_tool_history_before_csl_projection() {
        let temp = tempfile::tempdir().unwrap();
        let _journal_dir = session_journal::JournalDirGuard::new(temp.path());
        let session_id = format!("journal-canonical-{}", uuid::Uuid::new_v4());
        let owner_id = astra_services::local_owner_scope().id().to_string();
        let first_messages = vec![
            json!({"role": "user", "content": "inspect"}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call-1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "file body"}),
            json!({"role": "assistant", "content": "first answer"}),
        ];
        let active =
            astra_turn_core::active_conversation::ActiveConversation::empty(&owner_id, &session_id)
                .unwrap();
        let first = active
            .prepare_commit(1, None, first_messages.clone())
            .unwrap();
        let mut second_messages = first_messages;
        second_messages.extend([
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": "second answer"}),
        ]);
        let second = first
            .next
            .prepare_commit(2, None, second_messages.clone())
            .unwrap();
        let mut checkpoint = StepCheckpoint::heavy(
            "s2".to_string(),
            "t2".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.conversation_cursor = Some(second.next.cursor().clone());
        heavy.activated_deferred_tool_names = vec!["github".to_string()];
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            2,
            &checkpoint,
        )
        .unwrap();
        let writer = session_journal::JournalWriter::new(&session_id).unwrap();
        for (turn, commit) in [(1, first.commit), (2, second.commit)] {
            writer
                .append(
                    &session_journal::JournalEvent::turn(
                        Some(&session_id),
                        turn,
                        Some("test-model"),
                        "display user",
                        "display assistant",
                        0,
                        0,
                        0,
                        0,
                    )
                    .with_conversation_commit(commit),
                )
                .unwrap();
        }

        let continuation = super::load_session_continuation_for_recovery(&session_id)
            .expect("canonical journal should recover without waiting for CSL");

        assert_eq!(continuation.messages, second_messages);
        assert_eq!(
            continuation.active_conversation.source(),
            astra_turn_core::active_conversation::ActiveConversationSource::Journal
        );
        assert_eq!(continuation.active_conversation.cursor().completed_turn, 2);
        assert_eq!(continuation.messages[2]["role"], "tool");
        assert_eq!(continuation.messages[2]["content"], "file body");
        assert_eq!(
            continuation.activated_deferred_tool_names,
            vec!["github"],
            "an activation projection at the exact journal cursor is admissible"
        );
    }

    #[test]
    fn load_session_messages_returns_only_prompt_facing_checkpoint_messages() {
        let session_id = format!("test-session-cont-{}", uuid::Uuid::new_v4());
        let mut checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![
            json!({"role": "user", "content": "Remember: code is ZEBRA-99"}),
            json!({"role": "assistant", "content": "OK, noted."}),
        ];
        heavy.conversation_cursor = Some(exact_test_cursor(&session_id, 2, &heavy.messages));
        heavy.activated_deferred_tool_names = vec!["github".to_string()];
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            2,
            &checkpoint,
        )
        .unwrap();

        let continuation = super::load_session_continuation_for_recovery(&session_id);

        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        let continuation = continuation.expect("should load messages from checkpoint");
        assert_eq!(
            continuation.activated_deferred_tool_names,
            vec!["github"],
            "heavy fallback must carry activation even when compaction removed its original tool result"
        );
        let messages = continuation.messages;
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "Remember: code is ZEBRA-99");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"], "OK, noted.");
        assert!(
            messages.iter().all(|message| message["role"] != "system"),
            "runtime checkpoint budgets are recovery metadata, not conversation history"
        );
    }

    #[test]
    fn load_session_messages_returns_none_for_missing_session() {
        let messages = super::load_session_messages_for_continuation("nonexistent-session-xyz-42");
        assert!(messages.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn load_session_messages_uses_csl_when_checkpoint_errors() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("test-session-cont-corrupt-{}", uuid::Uuid::new_v4());
        let mut checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![
            json!({"role": "user", "content": "checkpoint history"}),
            json!({"role": "assistant", "content": "checkpoint answer"}),
        ];
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let path = astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            9,
            &checkpoint,
        )
        .unwrap();
        let encoded = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            encoded.replacen(
                &format!(r#""user_id":"{user_id}""#),
                r#""user_id":"wrong-owner""#,
                1,
            ),
        )
        .unwrap();

        let csl_messages = vec![
            json!({"role": "user", "content": "canonical question"}),
            json!({"role": "assistant", "content": "canonical answer"}),
        ];
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            9,
            &csl_messages,
            &exact_csl_state(&session_id, 9, &csl_messages),
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id)
            .expect("canonical CSL should not depend on checkpoint validity");

        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "canonical question");
        assert_eq!(messages[1]["content"], "canonical answer");
    }

    #[test]
    #[serial_test::serial]
    fn load_session_messages_prefers_csl_over_valid_checkpoint() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("test-session-csl-priority-{}", uuid::Uuid::new_v4());
        let user_id = crate::cli::cli_config::cli_utils::cli_user_id();
        let checkpoint = StepCheckpoint::heavy(
            "s1".to_string(),
            "t1".to_string(),
            "astra-cli".to_string(),
            ExecutionCursor::default(),
        );
        let mut checkpoint = checkpoint;
        let StepCheckpoint::Heavy(heavy) = &mut checkpoint else {
            unreachable!("StepCheckpoint::heavy must create a heavy checkpoint");
        };
        heavy.messages = vec![json!({"role": "user", "content": "older checkpoint"})];
        astra_pipeline::step_checkpoint::write_step_checkpoint(
            &user_id,
            &session_id,
            1,
            &checkpoint,
        )
        .unwrap();
        let semantics = astra_turn_types::UserTurnSemantics::new(
            astra_turn_types::ObjectiveRelation::Replace,
            None,
        );
        let mut canonical_current = json!({"role": "user", "content": "canonical current"});
        astra_turn_types::mark_user_turn_semantics(&mut canonical_current, semantics);
        let csl_messages = vec![
            canonical_current,
            json!({"role": "assistant", "content": "current answer"}),
        ];
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            2,
            &csl_messages,
            &exact_csl_state(&session_id, 2, &csl_messages),
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(
            astra_pipeline::step_checkpoint::owner_session_dir_for(&user_id, &session_id).unwrap(),
        );

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "canonical current");
        assert_eq!(
            astra_turn_types::user_turn_semantics(&messages[0]).expect("valid semantics"),
            Some(semantics)
        );
        assert_eq!(messages[1]["content"], "current answer");
    }

    #[test]
    #[serial_test::serial]
    fn load_session_messages_restores_completed_tool_evidence_from_csl() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("test-session-csl-tools-{}", uuid::Uuid::new_v4());
        let csl_messages = vec![
            json!({"role": "user", "content": "inspect Cargo.toml"}),
            json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"Cargo.toml\"}"
                        }
                    }]
            }),
            json!({
                    "role": "tool",
                    "tool_call_id": "call-1",
                    "content": "[package]\nname = \"astra\""
            }),
            json!({"role": "assistant", "content": "done"}),
        ];
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &csl_messages,
            &exact_csl_state(&session_id, 1, &csl_messages),
        )
        .unwrap();

        let messages = super::load_session_messages_for_continuation(&session_id)
            .expect("canonical continuation");

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["content"], "[package]\nname = \"astra\"");
    }

    #[test]
    #[serial_test::serial]
    fn csl_continuation_reports_corrupt_typed_metadata() {
        let (_sessions, _sessions_guard) = crate::tests::isolated_sessions_dir();
        let session_id = format!("test-session-csl-corrupt-{}", uuid::Uuid::new_v4());
        crate::cli::session::session_recovery::csl::write_full_csl_snapshot_atomic(
            &session_id,
            1,
            &[
                json!({
                    "role": "user",
                    "content": "canonical current",
                    (astra_turn_types::USER_TURN_SEMANTICS_FIELD): {
                        "schema_version": "invalid",
                        "objective_relation": "replace"
                    }
                }),
                json!({"role": "assistant", "content": "current answer"}),
            ],
            &astra_turn_core::conversation_log::SessionStateCompact::default(),
        )
        .unwrap();

        assert!(
            super::load_csl_continuation(&session_id).is_err(),
            "corrupt canonical metadata must not become an untyped continuation"
        );
    }

    #[test]
    fn sanitize_routes_by_runtime_ownership_not_message_text() {
        let ordinary =
            json!({"role": "user", "content": "## Already Fetched is literal user text"});
        let msgs = vec![
            json!({"role": "user", "content": "review code"}),
            json!({"role": "assistant", "content": "Here is the review..."}),
            astra_turn_types::runtime_owned_message(
                "system",
                "arbitrary runtime payload",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ),
            ordinary.clone(),
        ];
        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0]["content"], "review code");
        assert_eq!(result[1]["content"], "Here is the review...");
        assert_eq!(result[2], ordinary);
    }

    #[test]
    fn sanitize_keeps_canonical_turn_semantics_for_the_next_runtime() {
        let semantics = astra_turn_types::UserTurnSemantics::new(
            astra_turn_types::ObjectiveRelation::Replace,
            None,
        );
        let mut objective = json!({"role": "user", "content": "repair lifecycle"});
        astra_turn_types::mark_user_turn_semantics(&mut objective, semantics);

        let result = super::sanitize_continuation_messages(vec![
            objective,
            json!({"role": "assistant", "content": "working"}),
        ]);

        assert_eq!(
            astra_turn_types::user_turn_semantics(&result[0]).expect("valid semantics"),
            Some(semantics)
        );
    }

    #[test]
    fn sanitize_corrupt_turn_semantics_still_enforces_the_continuation_boundary() {
        let corrupt_field = astra_turn_types::USER_TURN_SEMANTICS_FIELD;
        let messages = vec![
            json!({"role": "user", "content": "stale objective"}),
            json!({"role": "system", "content": "boundary", "_compact_boundary": true}),
            astra_turn_types::runtime_owned_message(
                "system",
                "runtime-only retry instruction",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ),
            json!({
                "role": "user",
                "content": "current objective",
                (corrupt_field): {
                    "schema_version": "invalid",
                    "objective_relation": "replace"
                }
            }),
            json!({"role": "tool", "tool_call_id": "orphan", "content": "orphan result"}),
            json!({"role": "assistant", "content": "current answer"}),
        ];

        let sanitized = super::sanitize_continuation_messages(messages);

        assert_eq!(sanitized.len(), 2);
        assert_eq!(sanitized[0]["content"], "current objective");
        assert!(sanitized[0].get(corrupt_field).is_none());
        assert_eq!(sanitized[1]["content"], "current answer");
    }

    #[test]
    fn sanitize_compaction_boundary_drops_pre_boundary_stale_goal() {
        let msgs = vec![
            json!({"role": "user", "content": "3 agents 不同角度review这个分支的所有changes"}),
            json!({"role": "assistant", "content": "review summary"}),
            json!({"role": "system", "content": "arbitrary boundary", "_compact_boundary": true}),
            json!({"role": "user", "content": "不要review啊！"}),
            json!({"role": "assistant", "reasoning_content": "Maybe continue the old review"}),
            json!({"role": "tool", "content": "No matches found", "tool_call_id": "c1"}),
            json!({"role": "assistant", "content": "明白，不做 review。"}),
        ];

        let result = super::sanitize_continuation_messages(msgs);

        assert_eq!(result.len(), 2);
        assert_eq!(result[0]["content"], "不要review啊！");
        assert_eq!(result[1]["content"], "明白，不做 review。");
        assert!(
            result
                .iter()
                .all(|msg| !msg["content"].as_str().unwrap_or("").contains("3 agents"))
        );
    }

    #[test]
    fn history_pairs_use_user_visible_projection_not_prompt_recaps() {
        let msgs = vec![
            json!({"role": "user", "content": "continue\u{0}"}),
            astra_turn_types::runtime_owned_message(
                "system",
                "arbitrary tool trace",
                astra_turn_types::RuntimeMessageDelivery::EphemeralControl,
            ),
            astra_turn_types::runtime_owned_message(
                "system",
                "arbitrary recap",
                astra_turn_types::RuntimeMessageDelivery::Projection,
            ),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "\u{1b}[32mvisible answer\u{1b}[0m"}),
            json!({"role": "tool", "content": "raw tool payload"}),
        ];

        let pairs = super::history_pairs_from_messages(&msgs);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "continue");
        assert_eq!(pairs[0].1, "visible answer");
    }

    #[test]
    fn sanitize_preserves_trailing_completed_tool_round_for_pressure_aware_optimizer() {
        let msgs = vec![
            json!({"role": "user", "content": "check status"}),
            json!({"role": "assistant", "content": "Here is the status."}),
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"status\"}"}}]}),
            json!({"role": "tool", "content": "M file.rs", "tool_call_id": "1"}),
            json!({"role": "assistant", "tool_calls": [{"id": "2", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"diff\"}"}}]}),
            json!({"role": "tool", "content": "+line", "tool_call_id": "2"}),
        ];
        let result = super::sanitize_continuation_messages(msgs);
        assert_eq!(result.len(), 7);
        assert_eq!(result[2]["content"], "hi");
        assert_eq!(result[3]["tool_calls"][0]["id"], "1");
        assert_eq!(result[4]["tool_call_id"], "1");
        assert_eq!(result[5]["tool_calls"][0]["id"], "2");
        assert_eq!(result[6]["tool_call_id"], "2");
    }

    #[test]
    fn sanitize_preserves_complete_conversation() {
        let msgs = vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "Hi! How can I help?"}),
            json!({"role": "user", "content": "thanks"}),
            json!({"role": "assistant", "content": "You're welcome!"}),
        ];
        let result = super::sanitize_continuation_messages(msgs.clone());
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn sanitize_keeps_legitimate_system_checkmark_messages() {
        let msgs = vec![
            json!({"role": "system", "content": "✓ Deployment finished successfully."}),
            json!({"role": "assistant", "content": "done"}),
        ];

        let result = super::sanitize_continuation_messages(msgs);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["content"], "✓ Deployment finished successfully.");
    }

    #[test]
    fn history_pairs_drop_assistant_only_trace_and_preserve_structured_text() {
        let msgs = vec![
            json!({"role": "assistant", "content": "Earlier context compacted."}),
            json!({"role": "user", "content": "continue"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "Sure."}]}),
            json!({"role": "assistant", "tool_calls": [{"id": "1", "type": "function", "function": {"name": "bash", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "1", "content": "ok"}),
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Done."}]}),
        ];

        let pairs = super::history_pairs_from_messages(&msgs);

        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "continue");
        assert_eq!(pairs[0].1, "Sure.\n\nDone.");
    }
}
