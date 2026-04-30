//! Canonical paths for the thin client protocol (§5.5 + `router_builder`).

/// `POST` — chat turn as SSE (`data: {json}\\n\\n` per event).
pub const CHAT_STREAM: &str = "/chat/stream";

/// `POST` — low-level chat turn bridge (same SSE framing as stream).
pub const CHAT_TURN: &str = "/chat/turn";

/// `POST` — design doc: edge posts tool execution results (optional route).
pub const TOOLS_RESULT: &str = "/tools/result";

/// `POST` — design doc: user approval for gated tools.
pub const APPROVAL_RESPOND: &str = "/approval/respond";

/// `GET` — list durable runs.
pub const RUNS: &str = "/runs";

/// `POST` — edge registry (`edge_agent_registry` + JWT); body: [`crate::protocol::EdgeRegisterRequest`].
pub const AGENTS_EDGE: &str = "/agents/edge";

/// `POST` — edge heartbeat / liveness (paired with [`AGENTS_EDGE`]).
pub const AGENTS_EDGE_HEARTBEAT: &str = "/agents/edge/heartbeat";

pub const SESSIONS: &str = "/sessions";

/// `GET/PUT/DELETE /sessions/{id}`
#[inline]
pub fn session(id: &str) -> String {
    format!("/sessions/{id}")
}

#[inline]
pub fn session_close(id: &str) -> String {
    format!("/sessions/{id}/close")
}

#[inline]
pub fn session_replay(id: &str) -> String {
    format!("/sessions/{id}/replay")
}

#[inline]
pub fn session_replay_compare(id: &str) -> String {
    format!("/sessions/{id}/replay/compare")
}

/// Returns `None` if `artifact_kind` contains path-unsafe characters.
#[inline]
pub fn session_artifact_latest(session_id: &str, artifact_kind: &str) -> Option<String> {
    if !is_safe_path_segment(artifact_kind) {
        return None;
    }
    Some(format!(
        "/sessions/{session_id}/artifacts/latest/{artifact_kind}"
    ))
}

/// Returns `None` if `artifact_id` contains path-unsafe characters.
#[inline]
pub fn session_artifact_download(session_id: &str, artifact_id: &str) -> Option<String> {
    if !is_safe_path_segment(artifact_id) {
        return None;
    }
    Some(format!(
        "/sessions/{session_id}/artifacts/{artifact_id}/download"
    ))
}

/// A safe path segment contains only alphanumeric, `-`, `_`, or `.` characters
/// and is non-empty. Rejects `/`, `..`, `?`, `#`, `%`, etc.
fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

#[inline]
pub fn chat_session_reflect(session_id: &str) -> String {
    format!("/chat/session/{session_id}/reflect")
}

#[inline]
pub fn chat_session_decision_trace(session_id: &str) -> String {
    format!("/chat/session/{session_id}/decision-trace")
}

#[inline]
pub fn chat_run(run_id: &str) -> String {
    format!("/chat/runs/{run_id}")
}

#[inline]
pub fn chat_run_stream(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/stream")
}

#[inline]
pub fn chat_run_pause(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/pause")
}

#[inline]
pub fn chat_run_resume(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/resume")
}

#[inline]
pub fn chat_run_delegate(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/delegate")
}

#[inline]
pub fn chat_run_delegations(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/delegations")
}

#[inline]
pub fn chat_run_delegations_pause(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/delegations/pause")
}

#[inline]
pub fn chat_run_delegations_resume(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/delegations/resume")
}

pub const AUTH_REGISTER: &str = "/auth/register";
pub const AUTH_LOGIN: &str = "/auth/login";
pub const AUTH_REFRESH: &str = "/auth/refresh";
pub const AUTH_LOGOUT: &str = "/auth/logout";
pub const AUTH_ME: &str = "/auth/me";

pub const HEALTH: &str = "/health";

pub const MODELS: &str = "/models";

#[inline]
pub fn model(name: &str) -> String {
    format!("/models/{name}")
}

pub const SKILLS: &str = "/skills";

#[inline]
pub fn skill(id: &str) -> String {
    format!("/skills/{id}")
}

pub const SKILLS_STATUS: &str = "/skills/status";
pub const SKILLS_TEST: &str = "/skills/test";

/// Memory proxy routes (server uses POST for search).
pub const MEMORY_STORE: &str = "/memory/store";
pub const MEMORY_SEARCH: &str = "/memory/search";
pub const MEMORY_RETRIEVE: &str = "/memory/retrieve";
pub const MEMORY_PURGE: &str = "/memory/purge";

/// Task API (`router_builder`: list/create, detail, progress, status update).
pub const TASKS: &str = "/tasks";

#[inline]
pub fn task(id: &str) -> String {
    format!("/tasks/{id}")
}

#[inline]
pub fn task_progress(id: &str) -> String {
    format!("/tasks/{id}/progress")
}

#[inline]
pub fn task_status(id: &str) -> String {
    format!("/tasks/{id}/status")
}

/// `GET /tasks/{id}/lease` — current lease row (or null).
#[inline]
pub fn task_lease(id: &str) -> String {
    format!("/tasks/{id}/lease")
}

#[inline]
pub fn task_lease_claim(id: &str) -> String {
    format!("/tasks/{id}/lease/claim")
}

#[inline]
pub fn task_lease_release(id: &str) -> String {
    format!("/tasks/{id}/lease/release")
}

#[inline]
pub fn task_lease_renew(id: &str) -> String {
    format!("/tasks/{id}/lease/renew")
}

/// Context snapshots (`GET/POST /context`, `GET /context/{id}`).
pub const CONTEXT: &str = "/context";

#[inline]
pub fn context_capture(context_capture_id: &str) -> String {
    format!("/context/{context_capture_id}")
}

/// Non-streaming chat routing helper (server `chat_route_handler`).
pub const CHAT_ROUTE: &str = "/chat/route";

/// Lightweight LLM proxy for verification judge / edge components.
pub const COMPLETIONS: &str = "/v1/chat/completions";

// ── Admin API (`astra-admin-cli` → same server) ───────────────────────────────

pub const ADMIN_INIT: &str = "/admin/init";
pub const ADMIN_AUDIT: &str = "/admin/audit";
pub const ADMIN_USERS_GRANT_ROLE: &str = "/admin/users/grant-role";
pub const ADMIN_USERS_REVOKE_ROLE: &str = "/admin/users/revoke-role";
pub const ADMIN_TOKENS: &str = "/admin/tokens";
pub const ADMIN_PROMPTS_OPTIMIZE: &str = "/admin/prompts/optimize";
pub const ADMIN_FEEDBACK_STATS: &str = "/admin/feedback/stats";
pub const ADMIN_FEEDBACK_EXPORT: &str = "/admin/feedback/export";
pub const ADMIN_CONFIG: &str = "/admin/config";

#[inline]
pub fn model_check(model_name: &str) -> String {
    format!("/models/{model_name}/check")
}

#[inline]
pub fn admin_config_key(key: &str) -> String {
    format!("/admin/config/{key}")
}

#[inline]
pub fn skill_versions(skill_name: &str) -> String {
    format!("/skills/{skill_name}/versions")
}

// ── Session Audit paths ─────────────────────────────────────────────────────

/// `GET /audit/sessions` — cross-session list with filters.
pub const AUDIT_SESSIONS: &str = "/audit/sessions";

/// `GET /audit/stats` — aggregate stats across sessions.
pub const AUDIT_STATS: &str = "/audit/stats";

/// `GET /audit/tools` — cross-session tool analytics.
pub const AUDIT_TOOLS: &str = "/audit/tools";

/// `GET /sessions/{id}/audit/summary`
#[inline]
pub fn session_audit_summary(session_id: &str) -> String {
    format!("/sessions/{session_id}/audit/summary")
}

/// `GET /sessions/{id}/audit/turns`
#[inline]
pub fn session_audit_turns(session_id: &str) -> String {
    format!("/sessions/{session_id}/audit/turns")
}

/// `GET /sessions/{id}/audit/turns/{n}`
#[inline]
pub fn session_audit_turn_detail(session_id: &str, turn: u32) -> String {
    format!("/sessions/{session_id}/audit/turns/{turn}")
}

/// `GET /sessions/{id}/audit/tools`
#[inline]
pub fn session_audit_tools(session_id: &str) -> String {
    format!("/sessions/{session_id}/audit/tools")
}

/// `GET /sessions/{id}/audit/errors`
#[inline]
pub fn session_audit_errors(session_id: &str) -> String {
    format!("/sessions/{session_id}/audit/errors")
}

// ── Plan lifecycle endpoints (cloud-authoritative) ──────────────────────────

/// `POST /plans` / `GET /plans`
pub const PLANS: &str = "/plans";

#[inline]
pub fn plan(id: &str) -> String {
    format!("/plans/{id}")
}

#[inline]
pub fn plan_status(id: &str) -> String {
    format!("/plans/{id}/status")
}
#[inline]
pub fn plan_rewind(id: &str) -> String {
    format!("/plans/{id}/rewind")
}
/// `POST /plans/{id}/step-runs` (create) and `GET /plans/{id}/step-runs` (list).
/// Same path, different methods — read + write share the prefix for API
/// consumer intuition.
#[inline]
pub fn plan_step_runs(id: &str) -> String {
    format!("/plans/{id}/step-runs")
}

#[inline]
pub fn plan_step_run_completed(id: &str) -> String {
    format!("/plans/{id}/step-runs/completed")
}

#[inline]
pub fn plan_step_run_finish(plan_id: &str, run_id: &str) -> String {
    format!("/plans/{plan_id}/step-runs/{run_id}/finish")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Constants ---

    #[test]
    fn constants_start_with_slash() {
        for path in [
            CHAT_STREAM,
            CHAT_TURN,
            TOOLS_RESULT,
            APPROVAL_RESPOND,
            RUNS,
            AGENTS_EDGE,
            AGENTS_EDGE_HEARTBEAT,
            SESSIONS,
            AUTH_REGISTER,
            AUTH_LOGIN,
            AUTH_REFRESH,
            AUTH_LOGOUT,
            AUTH_ME,
            HEALTH,
            MODELS,
            SKILLS,
            SKILLS_STATUS,
            SKILLS_TEST,
            MEMORY_STORE,
            MEMORY_SEARCH,
            MEMORY_RETRIEVE,
            MEMORY_PURGE,
            TASKS,
            CONTEXT,
            CHAT_ROUTE,
            COMPLETIONS,
            ADMIN_INIT,
            ADMIN_AUDIT,
            ADMIN_USERS_GRANT_ROLE,
            ADMIN_USERS_REVOKE_ROLE,
            ADMIN_TOKENS,
            ADMIN_PROMPTS_OPTIMIZE,
            ADMIN_FEEDBACK_STATS,
            ADMIN_FEEDBACK_EXPORT,
            ADMIN_CONFIG,
            AUDIT_SESSIONS,
            AUDIT_STATS,
            AUDIT_TOOLS,
        ] {
            assert!(path.starts_with('/'), "path should start with /: {path}");
        }
    }

    // --- Session paths ---

    #[test]
    fn session_path() {
        assert_eq!(session("abc"), "/sessions/abc");
    }

    #[test]
    fn session_close_path() {
        assert_eq!(session_close("s1"), "/sessions/s1/close");
    }

    #[test]
    fn session_replay_path() {
        assert_eq!(session_replay("s1"), "/sessions/s1/replay");
    }

    #[test]
    fn session_replay_compare_path() {
        assert_eq!(session_replay_compare("s1"), "/sessions/s1/replay/compare");
    }

    #[test]
    fn session_artifact_latest_path() {
        assert_eq!(
            session_artifact_latest("s1", "llm_capture"),
            Some("/sessions/s1/artifacts/latest/llm_capture".to_string())
        );
    }

    #[test]
    fn session_artifact_download_path() {
        assert_eq!(
            session_artifact_download("s1", "a1"),
            Some("/sessions/s1/artifacts/a1/download".to_string())
        );
    }

    #[test]
    fn session_artifact_latest_rejects_path_traversal() {
        assert_eq!(session_artifact_latest("s1", "../../admin"), None);
        assert_eq!(session_artifact_latest("s1", "a/b"), None);
        assert_eq!(session_artifact_latest("s1", ".."), None);
        assert_eq!(session_artifact_latest("s1", ""), None);
        assert_eq!(session_artifact_latest("s1", "a?b"), None);
        assert_eq!(session_artifact_latest("s1", "a#b"), None);
    }

    #[test]
    fn session_artifact_download_rejects_path_traversal() {
        assert_eq!(session_artifact_download("s1", "../secret"), None);
        assert_eq!(session_artifact_download("s1", "a%2Fb"), None);
    }

    #[test]
    fn chat_session_reflect_path() {
        assert_eq!(chat_session_reflect("s1"), "/chat/session/s1/reflect");
    }

    #[test]
    fn chat_session_decision_trace_path() {
        assert_eq!(
            chat_session_decision_trace("s1"),
            "/chat/session/s1/decision-trace"
        );
    }

    #[test]
    fn chat_run_path() {
        assert_eq!(chat_run("r1"), "/chat/runs/r1");
    }

    #[test]
    fn chat_run_stream_path() {
        assert_eq!(chat_run_stream("r1"), "/chat/runs/r1/stream");
    }

    #[test]
    fn chat_run_pause_path() {
        assert_eq!(chat_run_pause("r1"), "/chat/runs/r1/pause");
    }

    #[test]
    fn chat_run_resume_path() {
        assert_eq!(chat_run_resume("r1"), "/chat/runs/r1/resume");
    }

    #[test]
    fn chat_run_delegate_path() {
        assert_eq!(chat_run_delegate("r1"), "/chat/runs/r1/delegate");
    }

    #[test]
    fn chat_run_delegations_path() {
        assert_eq!(chat_run_delegations("r1"), "/chat/runs/r1/delegations");
    }

    #[test]
    fn chat_run_delegations_pause_path() {
        assert_eq!(
            chat_run_delegations_pause("r1"),
            "/chat/runs/r1/delegations/pause"
        );
    }

    #[test]
    fn chat_run_delegations_resume_path() {
        assert_eq!(
            chat_run_delegations_resume("r1"),
            "/chat/runs/r1/delegations/resume"
        );
    }

    // --- Model/Skill paths ---

    #[test]
    fn model_path() {
        assert_eq!(model("gpt-4"), "/models/gpt-4");
    }

    #[test]
    fn skill_path() {
        assert_eq!(skill("bash"), "/skills/bash");
    }

    #[test]
    fn model_check_path() {
        assert_eq!(model_check("gpt-4"), "/models/gpt-4/check");
    }

    #[test]
    fn skill_versions_path() {
        assert_eq!(skill_versions("bash"), "/skills/bash/versions");
    }

    // --- Task paths ---

    #[test]
    fn task_path() {
        assert_eq!(task("t1"), "/tasks/t1");
    }

    #[test]
    fn task_progress_path() {
        assert_eq!(task_progress("t1"), "/tasks/t1/progress");
    }

    #[test]
    fn task_status_path() {
        assert_eq!(task_status("t1"), "/tasks/t1/status");
    }

    #[test]
    fn task_lease_path() {
        assert_eq!(task_lease("t1"), "/tasks/t1/lease");
    }

    #[test]
    fn task_lease_claim_path() {
        assert_eq!(task_lease_claim("t1"), "/tasks/t1/lease/claim");
    }

    #[test]
    fn task_lease_release_path() {
        assert_eq!(task_lease_release("t1"), "/tasks/t1/lease/release");
    }

    #[test]
    fn task_lease_renew_path() {
        assert_eq!(task_lease_renew("t1"), "/tasks/t1/lease/renew");
    }

    // --- Context path ---

    #[test]
    fn context_capture_path() {
        assert_eq!(context_capture("cap1"), "/context/cap1");
    }

    // --- Audit paths ---

    #[test]
    fn session_audit_summary_path() {
        assert_eq!(session_audit_summary("s1"), "/sessions/s1/audit/summary");
    }

    #[test]
    fn session_audit_turns_path() {
        assert_eq!(session_audit_turns("s1"), "/sessions/s1/audit/turns");
    }

    #[test]
    fn session_audit_turn_detail_path() {
        assert_eq!(
            session_audit_turn_detail("s1", 3),
            "/sessions/s1/audit/turns/3"
        );
    }

    #[test]
    fn session_audit_tools_path() {
        assert_eq!(session_audit_tools("s1"), "/sessions/s1/audit/tools");
    }

    #[test]
    fn session_audit_errors_path() {
        assert_eq!(session_audit_errors("s1"), "/sessions/s1/audit/errors");
    }

    // --- Edge cases ---

    #[test]
    fn empty_id() {
        assert_eq!(session(""), "/sessions/");
    }

    #[test]
    fn id_with_special_chars() {
        assert_eq!(task("a/b"), "/tasks/a/b");
    }
}
