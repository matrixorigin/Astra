use std::sync::Arc;

use serde_json::{Map, Value};

const PLAN_LOOKUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
const PLAN_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CachedAuthoring {
    #[default]
    Unchecked,
    Ready(bool),
    UnavailableUntil(tokio::time::Instant),
}

impl CachedAuthoring {
    fn current(self) -> Option<Option<bool>> {
        match self {
            Self::Ready(active) => Some(Some(active)),
            Self::UnavailableUntil(until) if tokio::time::Instant::now() < until => Some(None),
            Self::Unchecked | Self::UnavailableUntil(_) => None,
        }
    }
}

/// Snapshot used by the plan-mode write guard and the system-prompt
/// injector. Populated on first access per plan-mode state change; cleared
/// by the enter/exit tools so the next call sees fresh DB state.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlanModeSnapshot {
    /// Whether the session currently has an active plan still in authoring.
    pub(crate) authoring_active: CachedAuthoring,
    /// Rendered system-prompt section to inject on the next turn (`None`
    /// when there's no active plan or it's already executing).
    pub(crate) resume_hint: Option<String>,
}

/// Tools that mutate the world outside the session. Blocked while plan mode
/// is active (`PlanPhase` = Planning|Refining) to mirror the reference agent's
/// permission-overlay behaviour: the model must call ExitPlanMode before
/// writing anything.
///
/// Read-only tools (grep, glob, read_file, git action=status/diff/log and
/// web_search) stay available so the agent can continue exploring.
pub(crate) fn is_plan_mode_blocked_tool(tool: &str, args: &Value) -> bool {
    crate::turn::plan_mode_guard::is_plan_mode_blocked_tool(tool, args)
}

pub(crate) fn plan_mode_blocked_tool_result(tool: &str) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(format!(
        "Tool '{tool}' is blocked while plan mode is active. \
         Use read-only tools to finish the plan, then call \
         `exit_plan_mode(plan='...')` when ready. Existing permissions and \
         explicit review requests determine how authoring finishes."
    ));
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String("policy_denied".to_string()),
        ),
        (
            "rejection_code".to_string(),
            Value::String("plan_mode_active".to_string()),
        ),
        ("blocked".to_string(), Value::Bool(true)),
        ("retryable".to_string(), Value::Bool(false)),
    ]));
    result
}

pub(crate) fn plan_mode_unavailable_tool_result() -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(
        "Plan state could not be checked; this tool did not run. Retry after a few seconds when the service is available.".into(),
    );
    let mut metadata = Map::from_iter([
        (
            "error_kind".into(),
            Value::String(astra_core::ErrorKind::ToolUnavailable.as_str().into()),
        ),
        ("retryable".into(), Value::Bool(true)),
        (
            "retry_after_ms".into(),
            serde_json::json!(PLAN_LOOKUP_RETRY_DELAY.as_millis()),
        ),
    ]);
    astra_tools::execution_outcome::insert_not_executed_fact(&mut metadata);
    result.metadata = Some(metadata);
    result
}

pub(crate) async fn plan_mode_authoring_active(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    user_id: &str,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    authoring_active_handle: Option<&Arc<std::sync::RwLock<bool>>>,
) -> Option<bool> {
    if let Some(cached) = cache.read().await.authoring_active.current() {
        return cached;
    }
    // Serialize a cold refresh with explicit binding transitions so an old
    // read cannot publish an inactive result after a newer transition.
    let mut writer = cache.write().await;
    if let Some(cached) = writer.authoring_active.current() {
        return cached;
    }
    let Some((authoring, hint)) = tokio::time::timeout(
        PLAN_LOOKUP_TIMEOUT,
        recompute_plan_mode_snapshot(repo, user_id, session_id),
    )
    .await
    .ok()
    .flatten() else {
        // Bound retries during an outage without a background poll. A later
        // tool call can recover once the short cooldown has passed.
        writer.authoring_active = CachedAuthoring::UnavailableUntil(
            tokio::time::Instant::now() + PLAN_LOOKUP_RETRY_DELAY,
        );
        if let Some(handle) = authoring_active_handle
            && let Ok(mut active) = handle.write()
        {
            *active = true;
        }
        return None;
    };
    writer.authoring_active = CachedAuthoring::Ready(authoring);
    writer.resume_hint = hint;
    if let Some(handle) = authoring_active_handle
        && let Ok(mut active) = handle.write()
    {
        *active = authoring;
    }
    Some(authoring)
}

pub(crate) async fn recompute_plan_mode_snapshot(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    user_id: &str,
    session_id: &str,
) -> Option<(bool, Option<String>)> {
    let Some(repo) = repo else {
        return Some((false, None));
    };
    let plan_id = match repo.active_plan_for_session(user_id, session_id).await {
        Ok(Some(plan_id)) => plan_id,
        Ok(None) => return Some((false, None)),
        Err(error) => {
            tracing::warn!(%user_id, %session_id, %error, "plan mode: binding lookup failed; retaining write guard");
            return None;
        }
    };
    match repo.load(user_id, &plan_id).await {
        Ok(state) => {
            let authoring = astra_plan::plan_mode_authoring_active(&state);
            let hint = astra_plan::plan_resume_prompt_hint(&state);
            Some((authoring, hint))
        }
        Err(error) => {
            tracing::warn!(
                %user_id,
                %session_id,
                %plan_id,
                error = %error,
                "plan mode: active binding exists but draft load failed; retaining write guard"
            );
            None
        }
    }
}

pub(crate) async fn invalidate_plan_mode_cache(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    user_id: &str,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
    authoring_active_handle: Option<&Arc<std::sync::RwLock<bool>>>,
) -> Option<bool> {
    let mut writer = cache.write().await;
    *writer = PlanModeSnapshot::default();
    let Some((authoring, hint)) = tokio::time::timeout(
        PLAN_LOOKUP_TIMEOUT,
        recompute_plan_mode_snapshot(repo, user_id, session_id),
    )
    .await
    .ok()
    .flatten() else {
        writer.authoring_active = CachedAuthoring::UnavailableUntil(
            tokio::time::Instant::now() + PLAN_LOOKUP_RETRY_DELAY,
        );
        // A failed refresh cannot establish that writes are permitted.
        if let Some(handle) = authoring_active_handle
            && let Ok(mut slot) = handle.write()
        {
            *slot = true;
        }
        if let Some(handle) = resume_hint_handle
            && let Ok(mut slot) = handle.write()
        {
            *slot = None;
        }
        return None;
    };
    if let Some(handle) = resume_hint_handle
        && let Ok(mut slot) = handle.write()
    {
        *slot = hint.clone();
    }
    if let Some(handle) = authoring_active_handle
        && let Ok(mut slot) = handle.write()
    {
        *slot = authoring;
    }
    writer.authoring_active = CachedAuthoring::Ready(authoring);
    writer.resume_hint = hint;
    Some(authoring)
}

pub(crate) async fn execute_enter_plan_mode(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    session_id: &str,
    user_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
    authoring_active_handle: Option<&Arc<std::sync::RwLock<bool>>>,
    args: &Value,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> astra_tools::ToolResult {
    let Some(repo) = repo.cloned() else {
        return astra_tools::ToolResult::error(
            "Error: plan repository not configured on this executor".into(),
        );
    };
    let goal = args
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("(pending)")
        .trim()
        .to_string();
    let goal = if goal.is_empty() {
        "(pending)".to_string()
    } else {
        goal
    };

    let plan_id = args
        .get("plan_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| astra_plan::PlanModeState::generate_plan_id(&goal));

    const MAX_CAS_RETRIES: u32 = 3;
    let mut last_conflict: Option<String> = None;
    for _attempt in 0..MAX_CAS_RETRIES {
        let mut state = match repo.load(user_id, &plan_id).await {
            // Re-entering an existing plan changes only its binding. Saving
            // the same draft would needlessly advance the reviewed version,
            // including when another session still owns it.
            Ok(_) => {
                last_conflict = None;
                break;
            }
            Err(astra_plan::PlanLoadError::NotFound(_)) => {
                let mut state =
                    astra_plan::PlanModeState::new_with_owner(goal.clone(), user_id.to_string());
                state.session_hint = Some(session_id.to_string());
                state
            }
            Err(error) => {
                return astra_tools::ToolResult::error(format!("Error: load plan: {error}"));
            }
        };

        match repo.save(user_id, &plan_id, &mut state, None).await {
            Ok(()) => {
                last_conflict = None;
                break;
            }
            Err(astra_plan::PlanLoadError::Conflict { expected, actual }) => {
                last_conflict = Some(format!(
                    "version conflict (expected {expected}, stored {actual})"
                ));
                continue;
            }
            Err(error) => {
                return astra_tools::ToolResult::error(format!("Error: save plan: {error}"));
            }
        }
    }
    if let Some(conflict) = last_conflict {
        return astra_tools::ToolResult::error(format!(
            "Error: save plan after {MAX_CAS_RETRIES} retries: {conflict}"
        ));
    }

    // Fail closed before issuing the binding. If the caller is cancelled,
    // the operation is dropped with its owner and this executor stays guarded
    // until an explicit plan transition can refresh the durable state.
    let mut transition = cache.write().await;
    transition.authoring_active = CachedAuthoring::Ready(true);
    transition.resume_hint = None;
    if let Some(handle) = authoring_active_handle
        && let Ok(mut active) = handle.write()
    {
        *active = true;
    }
    let binding = repo.set_active_plan(user_id, session_id, Some(&plan_id));
    let binding = match cancel_token {
        Some(token) => tokio::select! {
            biased;
            result = binding => result,
            _ = token.cancelled() => return astra_tools::cancelled_tool_result("enter_plan_mode", true)
                .with_execution_outcome_unknown(),
        },
        None => binding.await,
    };
    drop(transition);
    if let Err(error) = binding {
        return match error {
            astra_plan::PlanLoadError::PlanInUse(_)
            | astra_plan::PlanLoadError::BindingConflict
            | astra_plan::PlanLoadError::SessionUnavailable
            | astra_plan::PlanLoadError::NotFound(_)
            | astra_plan::PlanLoadError::InvalidId(_) => {
                invalidate_plan_mode_cache(Some(&repo), user_id, session_id, cache,
                    resume_hint_handle, authoring_active_handle).await;
                astra_tools::ToolResult::error(format!("Error: cannot enter plan mode: {error}"))
            }
            _ => astra_tools::ToolResult::error(format!(
                "Error: could not confirm the plan binding: {error}. Check the current plan before retrying."
            )).with_execution_outcome_unknown(),
        };
    }
    invalidate_plan_mode_cache(
        Some(&repo),
        user_id,
        session_id,
        cache,
        resume_hint_handle,
        authoring_active_handle,
    )
    .await;

    if let Ok(writer) =
        astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
    {
        let _ = writer.append(
            &astra_services::session_journal::JournalEvent::plan_lifecycle(
                Some(session_id),
                "plan_mode_entered",
                Some(serde_json::json!({
                    "plan_id": plan_id,
                    "goal": goal,
                })),
            ),
        );
    }

    astra_tools::ToolResult::text(format!(
        "Entered plan mode. plan_id={} goal=\"{}\". Write tools are now blocked — \
         author the plan, then call `exit_plan_mode(plan='...')` when ready; existing permissions and explicit review requests govern execution.",
        plan_id, goal
    ))
}

pub(crate) async fn execute_exit_plan_mode(
    repo: Option<&Arc<dyn astra_plan::PlanRepository>>,
    user_id: &str,
    session_id: &str,
    cache: &tokio::sync::RwLock<PlanModeSnapshot>,
    resume_hint_handle: Option<&Arc<std::sync::RwLock<Option<String>>>>,
    authoring_active_handle: Option<&Arc<std::sync::RwLock<bool>>>,
    approval_gate: Option<&dyn astra_tools::ToolApprovalGate>,
    approval_request_id: &str,
    args: &Value,
) -> astra_tools::ToolResult {
    let Some(repo) = repo.cloned() else {
        return astra_tools::ToolResult::error(
            "Error: plan repository not configured on this executor".into(),
        );
    };

    let active = match repo
        .settled_active_plan_for_session(user_id, session_id)
        .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            return match invalidate_plan_mode_cache(
                Some(&repo), user_id, session_id, cache,
                resume_hint_handle, authoring_active_handle,
            ).await {
                Some(false) => astra_tools::ToolResult::text("Note: session has no active plan; nothing to exit.".into()),
                Some(true) => astra_tools::ToolResult::error("Error: the session's plan changed while exiting. Check the current plan.".into()),
                None => astra_tools::ToolResult::error("Error: the current plan state could not be confirmed. Check it before continuing.".into()),
            };
        }
        Err(error) => {
            return astra_tools::ToolResult::error(format!("Error: lookup active plan: {error}"));
        }
    };
    {
        let mut writer = cache.write().await;
        writer.authoring_active = CachedAuthoring::Ready(true);
        if let Some(handle) = authoring_active_handle
            && let Ok(mut active) = handle.write()
        {
            *active = true;
        }
    }

    let mut submitted_version = None;
    if let Some(plan_md) = args
        .get("plan")
        .and_then(Value::as_str)
        .or_else(|| args.get("plan_md").and_then(Value::as_str))
    {
        const MAX_CAS_RETRIES: u32 = 3;
        let mut last_conflict: Option<String> = None;
        for _attempt in 0..MAX_CAS_RETRIES {
            let mut state = match repo.load(user_id, &active).await {
                Ok(state) => state,
                Err(error) => {
                    return astra_tools::ToolResult::error(format!(
                        "Error: load active plan: {error}"
                    ));
                }
            };
            state.plan_md = Some(plan_md.to_string());
            let expected = Some(state.version);
            match repo.save(user_id, &active, &mut state, expected).await {
                Ok(()) => {
                    submitted_version = Some(state.version);
                    last_conflict = None;
                    break;
                }
                Err(astra_plan::PlanLoadError::Conflict { expected, actual }) => {
                    last_conflict = Some(format!(
                        "version conflict (expected {expected}, stored {actual})"
                    ));
                    continue;
                }
                Err(error) => {
                    return astra_tools::ToolResult::error(format!(
                        "Error: save submitted plan markdown: {error}"
                    ))
                    .with_execution_outcome_unknown();
                }
            }
        }
        if let Some(conflict) = last_conflict {
            return astra_tools::ToolResult::error(format!(
                "Error: save submitted plan markdown after {MAX_CAS_RETRIES} retries: {conflict}"
            ));
        }
    }

    let submitted_version = match submitted_version {
        Some(version) => version,
        None => match repo.load(user_id, &active).await {
            Ok(state) => state.version,
            Err(error) => {
                return astra_tools::ToolResult::error(format!(
                    "Error: load submitted plan: {error}"
                ));
            }
        },
    };

    let review_required = args.get("review").and_then(Value::as_bool).unwrap_or(false)
        || match approval_gate {
            Some(gate) => gate.plan_review_required().await,
            None => true,
        };
    let decision = if review_required {
        let Some(gate) = approval_gate else {
            invalidate_plan_mode_cache(
                Some(&repo),
                user_id,
                session_id,
                cache,
                resume_hint_handle,
                authoring_active_handle,
            )
            .await;
            return astra_tools::ToolResult::error(format!(
                "Error: Plan {active} was saved, but this run has no interactive approval channel. Reconnect from an interactive client and check the current plan before submitting it again."
            ));
        };
        if let Ok(writer) =
            astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
        {
            let _ = writer.append(
                &astra_services::session_journal::JournalEvent::plan_lifecycle(
                    Some(session_id),
                    "plan_submitted_for_approval",
                    Some(serde_json::json!({"plan_id":active})),
                ),
            );
        }
        Some(
            gate.request_approval(approval_request_id, "exit_plan_mode", args)
                .await,
        )
    } else {
        None
    };

    match decision {
        None | Some(astra_tools::ApprovalDecision::Approved) => {
            match repo
                .finish_authoring(user_id, session_id, &active, submitted_version)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    invalidate_plan_mode_cache(
                        Some(&repo),
                        user_id,
                        session_id,
                        cache,
                        resume_hint_handle,
                        authoring_active_handle,
                    )
                    .await;
                    return astra_tools::ToolResult::error("Error: The plan or session binding changed while it was being reviewed. Check the current plan before submitting again.".into());
                }
                Err(error) => {
                    return astra_tools::ToolResult::error(format!("Error: Plan {active} was saved, but authoring could not be confirmed: {error}. Check the current plan before continuing."))
                        .with_execution_outcome_unknown();
                }
            }
            let current = invalidate_plan_mode_cache(
                Some(&repo),
                user_id,
                session_id,
                cache,
                resume_hint_handle,
                authoring_active_handle,
            )
            .await;
            if let Ok(writer) =
                astra_services::session_journal::JournalWriter::for_user(user_id, session_id)
            {
                let _ = writer.append(
                    &astra_services::session_journal::JournalEvent::plan_lifecycle(
                        Some(session_id),
                        if review_required {
                            "plan_approved"
                        } else {
                            "plan_authoring_completed"
                        },
                        Some(serde_json::json!({ "plan_id": active })),
                    ),
                );
            }
            let output = if current == Some(true) {
                format!(
                    "Plan {active} finalized, but another plan is now active. Check the current plan before continuing."
                )
            } else if current.is_none() {
                format!(
                    "Plan {active} finalized, but the current plan state could not be confirmed. Check it before continuing."
                )
            } else if review_required {
                format!(
                    "Plan {active} approved. Plan mode is off and existing execution permissions apply."
                )
            } else {
                format!(
                    "Plan {active} finalized; continuing under existing authorization. Plan mode is off."
                )
            };
            astra_tools::ToolResult::text(output)
        }
        Some(astra_tools::ApprovalDecision::Denied { reason }) => {
            invalidate_plan_mode_cache(
                Some(&repo),
                user_id,
                session_id,
                cache,
                resume_hint_handle,
                authoring_active_handle,
            )
            .await;
            let reason = reason.unwrap_or_else(|| "the reviewer kept the plan in authoring".into());
            astra_tools::ToolResult::error(format!(
                "Plan {active} was not approved: {reason}. Check the current plan before submitting it again."
            ))
        }
        Some(astra_tools::ApprovalDecision::Timeout) => {
            invalidate_plan_mode_cache(
                Some(&repo),
                user_id,
                session_id,
                cache,
                resume_hint_handle,
                authoring_active_handle,
            )
            .await;
            astra_tools::ToolResult::error(format!(
                "Plan {active} approval timed out. Check the current plan and submit it again when an interactive reviewer is connected."
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_plan::PlanRepository;
    use astra_plan::{SubtaskPlan, TaskStatus};
    use serde_json::json;

    #[test]
    fn blocked_plan_write_points_to_the_current_review_policy() {
        let result = plan_mode_blocked_tool_result("write_file");
        assert!(result.is_error);
        assert_eq!(
            result.metadata.as_ref().unwrap()["error_kind"],
            "policy_denied"
        );
        assert!(result.output.contains("exit_plan_mode"));
        assert!(
            result
                .output
                .contains("Existing permissions and explicit review")
        );
        assert!(!result.output.contains("approval before write tools"));
    }

    struct ReviewGate {
        repo: Arc<dyn PlanRepository>,
        required: bool,
        edit_during_review: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl astra_tools::ToolApprovalGate for ReviewGate {
        async fn plan_review_required(&self) -> bool {
            self.required
        }
        fn requires_approval(&self, _: &str) -> bool {
            false
        }
        async fn request_approval(
            &self,
            _: &str,
            _: &str,
            _: &Value,
        ) -> astra_tools::ApprovalDecision {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.edit_during_review {
                let mut state = self.repo.load("u", "p").await.unwrap();
                let expected = state.version;
                state.plan_md = Some("newer draft".into());
                self.repo
                    .save("u", "p", &mut state, Some(expected))
                    .await
                    .unwrap();
            }
            astra_tools::ApprovalDecision::Approved
        }
    }

    #[tokio::test]
    async fn authoring_exit_preserves_policy_explicit_review_and_reviewed_revision() {
        let temp = tempfile::tempdir().unwrap();
        let _journal = astra_services::session_journal::JournalDirGuard::new(temp.path());
        for (required, explicit, edit) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, false, true),
        ] {
            let repo: Arc<dyn PlanRepository> = Arc::new(astra_plan::InMemoryPlanRepository::new());
            let mut state = astra_plan::PlanModeState::new_with_owner("goal".into(), "u".into());
            repo.save("u", "p", &mut state, None).await.unwrap();
            repo.set_active_plan("u", "s", Some("p")).await.unwrap();
            let gate = ReviewGate {
                repo: repo.clone(),
                required,
                edit_during_review: edit,
                calls: Default::default(),
            };
            let cache = tokio::sync::RwLock::new(PlanModeSnapshot::default());
            let result = execute_exit_plan_mode(
                Some(&repo),
                "u",
                "s",
                &cache,
                None,
                None,
                Some(&gate),
                "request",
                &json!({"plan":"submitted draft", "review":explicit}),
            )
            .await;
            assert_eq!(
                gate.calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(required || explicit)
            );
            let active = repo.active_plan_for_session("u", "s").await.unwrap();
            if edit {
                assert!(
                    result.is_error && result.output.starts_with("Error:"),
                    "{result:?}"
                );
                assert_eq!(active.as_deref(), Some("p"));
                assert_eq!(
                    repo.load("u", "p").await.unwrap().plan_md.as_deref(),
                    Some("newer draft")
                );
                assert_eq!(
                    plan_mode_authoring_active(Some(&repo), "u", "s", &cache, None).await,
                    Some(true)
                );
            } else {
                assert!(active.is_none(), "{result:?}");
                assert!(!result.is_error, "{result:?}");
                assert_eq!(result.output.contains("approved"), required || explicit);
                assert_eq!(
                    cache.read().await.authoring_active,
                    CachedAuthoring::Ready(false)
                );
            }
        }
    }

    #[tokio::test]
    async fn active_binding_keeps_write_guard_after_embedded_task_completion() {
        let repo: Arc<dyn PlanRepository> = Arc::new(astra_plan::InMemoryPlanRepository::new());
        let mut state = astra_plan::PlanModeState::new_with_owner(
            "Review release proposal".into(),
            "alice".into(),
        );
        state.plan.subtasks = vec![SubtaskPlan {
            id: "legacy-execution-projection".into(),
            title: "Do not infer approval from this row".into(),
            status: TaskStatus::Completed,
            ..Default::default()
        }];
        repo.save("alice", "plan-review", &mut state, None)
            .await
            .unwrap();
        repo.set_active_plan("alice", "session-1", Some("plan-review"))
            .await
            .unwrap();

        let (authoring, hint) = recompute_plan_mode_snapshot(Some(&repo), "alice", "session-1")
            .await
            .unwrap();
        assert!(
            authoring,
            "only trusted approval may release the write guard"
        );
        let hint = hint.expect("active draft hint");
        assert!(hint.contains("A plan draft is active"), "{hint}");
        assert!(!hint.contains("completed"), "{hint}");
    }

    #[tokio::test(start_paused = true)]
    async fn transient_binding_lookup_failure_is_not_cached_as_success_or_permanent_block() {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://root@127.0.0.1:1/astra")
            .unwrap();
        pool.close().await;
        let unavailable: Arc<dyn PlanRepository> =
            Arc::new(astra_plan::CloudPlanRepository::new(pool));
        let cache = tokio::sync::RwLock::new(PlanModeSnapshot::default());
        assert_eq!(
            plan_mode_authoring_active(Some(&unavailable), "u", "s", &cache, None).await,
            None
        );
        assert!(matches!(
            cache.read().await.authoring_active,
            CachedAuthoring::UnavailableUntil(_)
        ));

        let recovered: Arc<dyn PlanRepository> =
            Arc::new(astra_plan::InMemoryPlanRepository::new());
        assert_eq!(
            plan_mode_authoring_active(Some(&recovered), "u", "s", &cache, None).await,
            None
        );
        tokio::time::advance(PLAN_LOOKUP_RETRY_DELAY).await;
        assert_eq!(
            plan_mode_authoring_active(Some(&recovered), "u", "s", &cache, None).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn entering_another_sessions_plan_does_not_change_its_revision_or_owner() {
        let repo: Arc<dyn PlanRepository> = Arc::new(astra_plan::InMemoryPlanRepository::new());
        let mut draft = astra_plan::PlanModeState::new_with_owner("goal".into(), "user".into());
        repo.save("user", "plan", &mut draft, None).await.unwrap();
        repo.set_active_plan("user", "first", Some("plan"))
            .await
            .unwrap();
        let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot::default()));
        let result = execute_enter_plan_mode(
            Some(&repo),
            "second",
            "user",
            &cache,
            None,
            None,
            &json!({"goal": "goal", "plan_id": "plan"}),
            None,
        )
        .await;
        assert!(
            result.is_error && result.output.contains("active in a session"),
            "{result:?}"
        );
        assert_eq!(
            repo.load("user", "plan").await.unwrap().version,
            draft.version
        );
        assert_eq!(
            repo.active_plan_for_session("user", "first")
                .await
                .unwrap()
                .as_deref(),
            Some("plan")
        );
        assert_eq!(
            repo.active_plan_for_session("user", "second")
                .await
                .unwrap(),
            None
        );
    }

    struct PausedExit {
        inner: astra_plan::InMemoryPlanRepository,
        pause_lookup: std::sync::atomic::AtomicBool,
        pause_finish: std::sync::atomic::AtomicBool,
        observed: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl PlanRepository for PausedExit {
        async fn save(
            &self,
            user: &str,
            plan: &str,
            state: &mut astra_plan::PlanModeState,
            version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.save(user, plan, state, version).await
        }
        async fn load(
            &self,
            user: &str,
            plan: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.inner.load(user, plan).await
        }
        async fn list_for_user(
            &self,
            user: &str,
            filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            self.inner.list_for_user(user, filter).await
        }
        async fn delete(&self, user: &str, plan: &str) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.delete(user, plan).await
        }
        async fn set_active_plan(
            &self,
            user: &str,
            session: &str,
            plan: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.set_active_plan(user, session, plan).await
        }
        async fn finish_authoring(
            &self,
            user: &str,
            session: &str,
            plan: &str,
            version: u64,
        ) -> Result<bool, astra_plan::PlanLoadError> {
            let result = self
                .inner
                .finish_authoring(user, session, plan, version)
                .await;
            if self
                .pause_finish
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.observed.notify_one();
                self.resume.notified().await;
            }
            result
        }
        async fn active_plan_for_session(
            &self,
            user: &str,
            session: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            let result = self.inner.active_plan_for_session(user, session).await;
            if self
                .pause_lookup
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.observed.notify_one();
                self.resume.notified().await;
            }
            result
        }
    }

    #[tokio::test]
    async fn stalled_invalidation_releases_waiting_write_checks_and_can_recover() {
        let backing = Arc::new(PausedExit {
            inner: astra_plan::InMemoryPlanRepository::new(),
            pause_lookup: std::sync::atomic::AtomicBool::new(true),
            pause_finish: std::sync::atomic::AtomicBool::new(false),
            observed: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let repo: Arc<dyn PlanRepository> = backing.clone();
        let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot::default()));
        let handle = Arc::new(std::sync::RwLock::new(false));
        let hint = Arc::new(std::sync::RwLock::new(Some("stale plan".to_string())));
        let invalidating = {
            let (repo, cache, handle, hint) =
                (repo.clone(), cache.clone(), handle.clone(), hint.clone());
            tokio::spawn(async move {
                invalidate_plan_mode_cache(
                    Some(&repo),
                    "u",
                    "s",
                    &cache,
                    Some(&hint),
                    Some(&handle),
                )
                .await
            })
        };
        backing.observed.notified().await;
        let checking = {
            let (repo, cache) = (repo.clone(), cache.clone());
            tokio::spawn(async move {
                plan_mode_authoring_active(Some(&repo), "u", "s", &cache, None).await
            })
        };
        let bound = PLAN_LOOKUP_TIMEOUT + std::time::Duration::from_secs(2);
        assert_eq!(
            tokio::time::timeout(bound, invalidating)
                .await
                .unwrap()
                .unwrap(),
            None
        );
        assert_eq!(
            tokio::time::timeout(bound, checking)
                .await
                .unwrap()
                .unwrap(),
            None
        );
        assert!(*handle.read().unwrap());
        assert!(hint.read().unwrap().is_none());
        assert!(matches!(
            cache.read().await.authoring_active,
            CachedAuthoring::UnavailableUntil(_)
        ));
        cache.write().await.authoring_active = CachedAuthoring::Unchecked;
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "u", "s", &cache, None).await,
            Some(false)
        );
    }

    #[tokio::test]
    async fn plan_exit_does_not_clear_a_newer_binding() {
        let temp = tempfile::tempdir().unwrap();
        let _journal = astra_services::session_journal::JournalDirGuard::new(temp.path());
        for finish_existing in [false, true] {
            let backing = Arc::new(PausedExit {
                inner: astra_plan::InMemoryPlanRepository::new(),
                pause_lookup: std::sync::atomic::AtomicBool::new(!finish_existing),
                pause_finish: std::sync::atomic::AtomicBool::new(finish_existing),
                observed: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let repo: Arc<dyn PlanRepository> = backing.clone();
            if finish_existing {
                let mut state = astra_plan::PlanModeState::new_with_owner("old".into(), "u".into());
                repo.save("u", "old", &mut state, None).await.unwrap();
                repo.set_active_plan("u", "s", Some("old")).await.unwrap();
            }
            let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot {
                authoring_active: CachedAuthoring::Ready(finish_existing),
                resume_hint: None,
            }));
            let shared = Arc::new(std::sync::RwLock::new(finish_existing));
            let exit = {
                let repo = repo.clone();
                let cache = cache.clone();
                let shared = shared.clone();
                tokio::spawn(async move {
                    let gate = ReviewGate {
                        repo: repo.clone(),
                        required: false,
                        edit_during_review: false,
                        calls: Default::default(),
                    };
                    execute_exit_plan_mode(
                        Some(&repo),
                        "u",
                        "s",
                        &cache,
                        None,
                        Some(&shared),
                        Some(&gate),
                        "request",
                        &json!({}),
                    )
                    .await
                })
            };
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                backing.observed.notified(),
            )
            .await
            .unwrap();
            let entered = execute_enter_plan_mode(
                Some(&repo),
                "s",
                "u",
                &cache,
                None,
                Some(&shared),
                &json!({"goal":"new", "plan_id":"new"}),
                None,
            )
            .await;
            assert!(
                !entered.is_error && entered.output.starts_with("Entered plan mode"),
                "{entered:?}"
            );
            backing.resume.notify_one();
            let result = exit.await.unwrap();
            assert!(!result.output.contains("Plan mode is off"), "{result:?}");
            assert!(
                result.output.contains("plan") && result.output.contains("active")
                    || result.output.contains("plan changed"),
                "{result:?}"
            );
            assert_eq!(
                plan_mode_authoring_active(Some(&repo), "u", "s", &cache, None).await,
                Some(true)
            );
            assert!(*shared.read().unwrap());
        }
    }

    struct LostBindAcknowledgement {
        inner: astra_plan::InMemoryPlanRepository,
        lookup_unavailable: std::sync::atomic::AtomicBool,
        pause_before_commit: bool,
        pause_after_commit: bool,
        started: tokio::sync::Notify,
        committed: tokio::sync::Notify,
        allow_commit: tokio::sync::Notify,
        allow_ack: tokio::sync::Notify,
    }

    impl LostBindAcknowledgement {
        fn new(pause_before_commit: bool, pause_after_commit: bool) -> Self {
            Self {
                inner: astra_plan::InMemoryPlanRepository::new(),
                lookup_unavailable: std::sync::atomic::AtomicBool::new(false),
                pause_before_commit,
                pause_after_commit,
                started: tokio::sync::Notify::new(),
                committed: tokio::sync::Notify::new(),
                allow_commit: tokio::sync::Notify::new(),
                allow_ack: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl PlanRepository for LostBindAcknowledgement {
        async fn save(
            &self,
            user: &str,
            plan: &str,
            state: &mut astra_plan::PlanModeState,
            version: Option<u64>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.save(user, plan, state, version).await
        }
        async fn load(
            &self,
            user: &str,
            plan: &str,
        ) -> Result<astra_plan::PlanModeState, astra_plan::PlanLoadError> {
            self.inner.load(user, plan).await
        }
        async fn list_for_user(
            &self,
            user: &str,
            filter: astra_plan::PlanListFilter<'_>,
        ) -> Result<Vec<astra_plan::SavedPlanInfo>, astra_plan::PlanLoadError> {
            self.inner.list_for_user(user, filter).await
        }
        async fn delete(&self, user: &str, plan: &str) -> Result<(), astra_plan::PlanLoadError> {
            self.inner.delete(user, plan).await
        }
        async fn set_active_plan(
            &self,
            user: &str,
            session: &str,
            plan: Option<&str>,
        ) -> Result<(), astra_plan::PlanLoadError> {
            self.started.notify_one();
            if self.pause_before_commit {
                self.allow_commit.notified().await;
            }
            self.inner.set_active_plan(user, session, plan).await?;
            self.lookup_unavailable
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.committed.notify_one();
            if self.pause_after_commit {
                self.allow_ack.notified().await;
            }
            Err(astra_plan::PlanLoadError::Internal(
                "commit acknowledgement lost".into(),
            ))
        }
        async fn finish_authoring(
            &self,
            user: &str,
            session: &str,
            plan: &str,
            version: u64,
        ) -> Result<bool, astra_plan::PlanLoadError> {
            self.inner
                .finish_authoring(user, session, plan, version)
                .await
        }
        async fn active_plan_for_session(
            &self,
            user: &str,
            session: &str,
        ) -> Result<Option<String>, astra_plan::PlanLoadError> {
            if self
                .lookup_unavailable
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(astra_plan::PlanLoadError::Internal(
                    "lookup unavailable".into(),
                ));
            }
            self.inner.active_plan_for_session(user, session).await
        }
    }

    #[tokio::test]
    async fn lost_bind_acknowledgement_never_reuses_an_inactive_guard() {
        let backing = Arc::new(LostBindAcknowledgement::new(false, false));
        let repo: Arc<dyn PlanRepository> = backing.clone();
        let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot {
            authoring_active: CachedAuthoring::Ready(false),
            resume_hint: None,
        }));
        let shared = Arc::new(std::sync::RwLock::new(false));
        let result = execute_enter_plan_mode(
            Some(&repo),
            "session",
            "user",
            &cache,
            None,
            Some(&shared),
            &json!({"goal": "goal", "plan_id": "plan"}),
            None,
        )
        .await;
        assert!(
            result.is_error && result.output.contains("could not confirm"),
            "{result:?}"
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("execution_fact")),
            Some(&json!("unknown"))
        );
        assert!(*shared.read().unwrap());
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &cache, None).await,
            Some(true)
        );
        backing
            .lookup_unavailable
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &cache, None).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn cancelled_bind_after_commit_does_not_reuse_an_inactive_guard() {
        let backing = Arc::new(LostBindAcknowledgement::new(false, true));
        let repo: Arc<dyn PlanRepository> = backing.clone();
        let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot {
            authoring_active: CachedAuthoring::Ready(false),
            resume_hint: None,
        }));
        let shared = Arc::new(std::sync::RwLock::new(false));
        let task = {
            let repo = repo.clone();
            let cache = cache.clone();
            let shared = shared.clone();
            tokio::spawn(async move {
                execute_enter_plan_mode(
                    Some(&repo),
                    "session",
                    "user",
                    &cache,
                    None,
                    Some(&shared),
                    &json!({"goal": "goal", "plan_id": "plan"}),
                    None,
                )
                .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            backing.committed.notified(),
        )
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        backing.allow_ack.notify_one();
        assert!(*shared.read().unwrap());
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &cache, None).await,
            Some(true)
        );
        backing
            .lookup_unavailable
            .store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &cache, None).await,
            Some(true)
        );
    }

    #[tokio::test]
    async fn cancelled_bind_cannot_commit_after_its_owner_is_dropped() {
        let backing = Arc::new(LostBindAcknowledgement::new(true, false));
        let repo: Arc<dyn PlanRepository> = backing.clone();
        let cache = Arc::new(tokio::sync::RwLock::new(PlanModeSnapshot {
            authoring_active: CachedAuthoring::Ready(false),
            resume_hint: None,
        }));
        let shared = Arc::new(std::sync::RwLock::new(false));
        let cancel = tokio_util::sync::CancellationToken::new();
        let caller = {
            let repo = repo.clone();
            let cache = cache.clone();
            let shared = shared.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                execute_enter_plan_mode(
                    Some(&repo),
                    "session",
                    "user",
                    &cache,
                    None,
                    Some(&shared),
                    &json!({"goal": "goal", "plan_id": "plan"}),
                    Some(&cancel),
                )
                .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            backing.started.notified(),
        )
        .await
        .unwrap();
        cancel.cancel();
        let outcome = caller.await.unwrap();
        assert!(outcome.is_error);
        assert_eq!(
            outcome
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("execution_fact")),
            Some(&json!("unknown"))
        );
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &cache, None).await,
            Some(true)
        );
        backing.allow_commit.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(
            backing
                .inner
                .active_plan_for_session("user", "session")
                .await
                .unwrap(),
            None
        );
        let successor = tokio::sync::RwLock::new(PlanModeSnapshot::default());
        assert_eq!(
            plan_mode_authoring_active(Some(&repo), "user", "session", &successor, None).await,
            Some(false)
        );
    }

    #[test]
    fn plan_mode_blocks_all_write_and_execute_class_tools() {
        for (tool, args) in [
            ("bash", json!({"command": "touch plan.txt"})),
            ("background_shell", json!({"command": "ls src"})),
            ("powershell", json!({"command": "Get-ChildItem"})),
            ("write_file", json!({"path": "plan.txt", "content": "x"})),
            (
                "str_replace",
                json!({"path": "plan.txt", "old": "a", "new": "b"}),
            ),
            ("multi_edit", json!({"path": "plan.txt", "edits": []})),
            ("delete_file", json!({"path": "plan.txt"})),
            ("rollback_file_edits", json!({"scope": "current_turn"})),
            ("rollback_session_state", json!({"scope": "last_turn"})),
            ("adjust_config", json!({"key": "model", "value": "fast"})),
            ("compress_context", json!({"target_tokens": 1000})),
            ("run_script", json!({"script": "touch plan.txt"})),
            ("rollback_database_snapshots", json!({})),
        ] {
            assert!(
                is_plan_mode_blocked_tool(tool, &args),
                "{tool} must be blocked during plan authoring"
            );
        }
    }

    #[test]
    fn plan_mode_allows_read_only_exploration_by_args() {
        for (tool, args) in [
            ("read_file", json!({"path": "src/lib.rs"})),
            ("bash", json!({"command": "git status --short"})),
            ("bash", json!({"command": "ls src"})),
            ("grep", json!({"pattern": "needle", "path": "src"})),
            ("glob", json!({"pattern": "**/*.rs"})),
            ("list_dir", json!({"path": "src"})),
            (
                "memory",
                json!({"action": "remember", "content": "plan context"}),
            ),
        ] {
            assert!(
                !is_plan_mode_blocked_tool(tool, &args),
                "{tool} with args {args} should remain available during plan authoring"
            );
        }
    }

    #[test]
    fn plan_mode_blocks_action_scoped_tools_only_when_mutating() {
        assert!(is_plan_mode_blocked_tool(
            "task_stop",
            &json!({"task_id": "bg-shell-1"})
        ));
        assert!(is_plan_mode_blocked_tool(
            "worktree",
            &json!({"action": "enter"})
        ));
    }
}
