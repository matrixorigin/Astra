//! Canonical paths for the thin client protocol (§5.5 + `router_builder`).

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Encode model names as one URL path segment while preserving the standard
/// unreserved characters. Model names are provider data and may contain `/`,
/// `?`, or `#`; interpolating them directly changes the route identity.
const MODEL_SEGMENT_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[inline]
fn model_segment(name: &str) -> String {
    utf8_percent_encode(name, MODEL_SEGMENT_ENCODE_SET).to_string()
}

/// `POST` — chat turn as SSE (`data: {json}\\n\\n` per event).
pub const CHAT_STREAM: &str = "/chat/stream";

/// `POST` — design doc: edge posts tool execution results (optional route).
pub const TOOLS_RESULT: &str = "/tools/result";

/// `POST` — design doc: user approval for gated tools.
pub const APPROVAL_RESPOND: &str = "/approval/respond";

/// `POST` — answer or cancel a durable `ask_user` interaction.
pub const USER_PROMPT_RESPOND: &str = "/user-prompts/respond";

/// `POST` — submit or cancel an opaque provider interaction.
pub const PROVIDER_INTERACTION_RESPOND: &str = "/provider-interactions/respond";

/// `GET` — list durable runs.
pub const RUNS: &str = "/runs";

/// Canonical Work catalog and Start Work boundary.
pub const WORKS: &str = "/v1/works";

/// Resolve an already-known internal session to its public Work identity.
#[inline]
pub fn work_session_binding(session_id: &str) -> Option<String> {
    is_safe_path_segment(session_id).then(|| format!("{WORKS}/session-bindings/{session_id}"))
}

/// Exact Work resource path. Work identities are path segments, never
/// caller-supplied path fragments.
#[inline]
pub fn work(work_id: &str) -> Option<String> {
    is_safe_path_segment(work_id).then(|| format!("{WORKS}/{work_id}"))
}

/// Establish one server-owned attachment to a Work branch.
#[inline]
pub fn work_branch_attachments(work_id: &str, branch_id: &str) -> Option<String> {
    if !is_safe_path_segment(work_id) || !is_safe_path_segment(branch_id) {
        return None;
    }
    Some(format!(
        "{WORKS}/{work_id}/branches/{branch_id}/attachments"
    ))
}

/// Release one exact Work branch attachment.
#[inline]
pub fn work_branch_attachment(
    work_id: &str,
    branch_id: &str,
    attachment_id: &str,
) -> Option<String> {
    if !is_safe_path_segment(work_id)
        || !is_safe_path_segment(branch_id)
        || !is_safe_path_segment(attachment_id)
    {
        return None;
    }
    Some(format!(
        "{WORKS}/{work_id}/branches/{branch_id}/attachments/{attachment_id}"
    ))
}

/// Submit a typed Work branch controller operation.
#[inline]
pub fn work_branch_control_operations(work_id: &str, branch_id: &str) -> Option<String> {
    if !is_safe_path_segment(work_id) || !is_safe_path_segment(branch_id) {
        return None;
    }
    Some(format!(
        "{WORKS}/{work_id}/branches/{branch_id}/control-operations"
    ))
}

/// Continue one Work branch without exposing its internal session identity.
#[inline]
pub fn work_branch_turns(work_id: &str, branch_id: &str) -> Option<String> {
    if !is_safe_path_segment(work_id) || !is_safe_path_segment(branch_id) {
        return None;
    }
    Some(format!("{WORKS}/{work_id}/branches/{branch_id}/turns"))
}

/// Read the bounded canonical Task Graph for one Work branch.
#[inline]
pub fn work_branch_task_graph(work_id: &str, branch_id: &str) -> Option<String> {
    if !is_safe_path_segment(work_id) || !is_safe_path_segment(branch_id) {
        return None;
    }
    Some(format!("{WORKS}/{work_id}/branches/{branch_id}/task-graph"))
}

/// `GET` — optional tool capacity from server and connected edge providers.
pub const RUNTIME_CAPABILITIES: &str = "/runtime/capabilities";

/// `GET` — latest durable LLM intent-drift assessment for a session.
pub const INTROSPECTION_DRIFT_CHECK: &str = "/introspection/drift-check";

/// `POST` — edge registry (`edge_agent_registry` + JWT); body: [`crate::protocol::EdgeRegisterRequest`].
pub const AGENTS_EDGE: &str = "/agents/edge";

/// `POST` — edge heartbeat / liveness (paired with [`AGENTS_EDGE`]).
pub const AGENTS_EDGE_HEARTBEAT: &str = "/agents/edge/heartbeat";

/// `POST/GET` — automation event facts (`agent_events`).
pub const EVENTS: &str = "/events";
pub const SYNC_OUTBOX_EVENTS: &str = "/sync/outbox/events";

#[inline]
pub fn event(event_id: &str) -> String {
    format!("/events/{event_id}")
}

pub const SESSIONS: &str = "/sessions";
pub const SESSIONS_RESUMABLE: &str = "/sessions/resumable";

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
pub fn session_cancel(id: &str) -> String {
    format!("/sessions/{id}/cancel")
}

#[inline]
pub fn session_resume(id: &str) -> String {
    format!("/sessions/{id}/resume")
}

#[inline]
pub fn session_state(id: &str) -> String {
    format!("/sessions/{id}/state")
}

#[inline]
pub fn session_device_enroll(id: &str) -> String {
    format!("/sessions/{id}/device/enroll")
}

#[inline]
pub fn session_device_challenge(id: &str) -> String {
    format!("/sessions/{id}/device/challenge")
}

#[inline]
pub fn session_device_trust(id: &str) -> String {
    format!("/sessions/{id}/device/trust")
}

#[inline]
pub fn session_device_revoke(id: &str) -> String {
    format!("/sessions/{id}/device/revoke")
}

#[inline]
pub fn session_runs(id: &str) -> String {
    format!("/sessions/{id}/runs")
}

#[inline]
pub fn session_transcript(id: &str) -> String {
    format!("/sessions/{id}/transcript")
}

#[inline]
pub fn session_artifacts(id: &str) -> String {
    format!("/sessions/{id}/artifacts")
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
pub fn session_artifact(session_id: &str, artifact_id: &str) -> Option<String> {
    if !is_safe_path_segment(artifact_id) {
        return None;
    }
    Some(format!("/sessions/{session_id}/artifacts/{artifact_id}"))
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
pub fn chat_run_user_intents(run_id: &str) -> String {
    format!("/chat/runs/{run_id}/intents")
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
pub const AUTH_MEMORIA: &str = "/auth/memoria";
pub const AUTH_REFRESH: &str = "/auth/refresh";
pub const AUTH_LOGOUT: &str = "/auth/logout";
pub const AUTH_REAUTHENTICATE: &str = "/auth/reauthenticate";
pub const AUTH_ME: &str = "/auth/me";

pub const HEALTH: &str = "/health";

pub const MODELS: &str = "/models";
pub const MODEL_ACCESS: &str = "/model-access";

#[inline]
pub fn model(name: &str) -> String {
    format!("/models/{}", model_segment(name))
}

#[inline]
pub fn model_memory() -> &'static str {
    "/models/memory"
}

#[inline]
pub fn model_check(model_name: &str) -> String {
    format!("/models/{}/check", model_segment(model_name))
}

pub const SKILLS: &str = "/skills";

#[inline]
pub fn skill(id: &str) -> String {
    format!("/skills/{id}")
}

pub const SKILLS_STATUS: &str = "/skills/status";

/// Memory proxy routes (server uses POST for search).
pub const MEMORY_STORE: &str = "/memory/store";
pub const MEMORY_SEARCH: &str = "/memory/search";
pub const MEMORY_RETRIEVE: &str = "/memory/retrieve";
pub const MEMORY_PURGE: &str = "/memory/purge";

/// Context snapshots (`GET/POST /context`, `GET /context/{id}`).
pub const CONTEXT: &str = "/context";

#[inline]
pub fn context_capture(context_capture_id: &str) -> String {
    format!("/context/{context_capture_id}")
}

/// Lightweight LLM proxy for verification judge / edge components.
pub const COMPLETIONS: &str = "/v1/chat/completions";

// ── Admin API (`astra-cli` → same server) ───────────────────────────────

pub const ADMIN_INIT: &str = "/admin/init";
pub const ADMIN_REGISTER: &str = "/admin/register";
pub const ADMIN_AUDIT: &str = "/admin/audit";
pub const ADMIN_USERS_GRANT_ROLE: &str = "/admin/users/grant-role";
pub const ADMIN_USERS_REVOKE_ROLE: &str = "/admin/users/revoke-role";
pub const ADMIN_TOKENS: &str = "/admin/tokens";
pub const ADMIN_PROMPTS_OPTIMIZE: &str = "/admin/prompts/optimize";
pub const ADMIN_FEEDBACK_STATS: &str = "/admin/feedback/stats";
pub const ADMIN_FEEDBACK_EXPORT: &str = "/admin/feedback/export";
pub const ADMIN_CONFIG: &str = "/admin/config";

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

#[cfg(test)]
mod tests {
    use super::*;

    // --- Constants ---

    #[test]
    fn constants_start_with_slash() {
        for path in [
            CHAT_STREAM,
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
            AUTH_REAUTHENTICATE,
            AUTH_ME,
            HEALTH,
            MODELS,
            SKILLS,
            SKILLS_STATUS,
            MEMORY_STORE,
            MEMORY_SEARCH,
            MEMORY_RETRIEVE,
            MEMORY_PURGE,
            CONTEXT,
            COMPLETIONS,
            ADMIN_INIT,
            ADMIN_REGISTER,
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
    fn session_cancel_path() {
        assert_eq!(session_cancel("s1"), "/sessions/s1/cancel");
    }

    #[test]
    fn session_state_path() {
        assert_eq!(session_state("s1"), "/sessions/s1/state");
    }

    #[test]
    fn session_device_paths_are_one_explicit_protocol_family() {
        assert_eq!(session_device_enroll("s1"), "/sessions/s1/device/enroll");
        assert_eq!(
            session_device_challenge("s1"),
            "/sessions/s1/device/challenge"
        );
        assert_eq!(session_device_trust("s1"), "/sessions/s1/device/trust");
        assert_eq!(session_device_revoke("s1"), "/sessions/s1/device/revoke");
    }

    #[test]
    fn session_runs_path() {
        assert_eq!(session_runs("s1"), "/sessions/s1/runs");
    }

    #[test]
    fn session_transcript_path() {
        assert_eq!(session_transcript("s1"), "/sessions/s1/transcript");
    }

    #[test]
    fn session_artifacts_path() {
        assert_eq!(session_artifacts("s1"), "/sessions/s1/artifacts");
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
            session_artifact("s1", "a1"),
            Some("/sessions/s1/artifacts/a1".to_string())
        );
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
        assert_eq!(session_artifact("s1", "../secret"), None);
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
    fn chat_run_user_intents_path() {
        assert_eq!(chat_run_user_intents("r1"), "/chat/runs/r1/intents");
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
        assert_eq!(
            model("bedrock/claude?variant#1"),
            "/models/bedrock%2Fclaude%3Fvariant%231"
        );
        assert_eq!(MODEL_ACCESS, "/model-access");
    }

    #[test]
    fn skill_path() {
        assert_eq!(skill("bash"), "/skills/bash");
    }

    #[test]
    fn model_check_path() {
        assert_eq!(model_check("gpt-4"), "/models/gpt-4/check");
        assert_eq!(
            model_check("bedrock/claude"),
            "/models/bedrock%2Fclaude/check"
        );
    }

    #[test]
    fn skill_versions_path() {
        assert_eq!(skill_versions("bash"), "/skills/bash/versions");
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
    fn work_paths_accept_resource_ids_and_reject_path_fragments() {
        assert_eq!(work("work-1").as_deref(), Some("/v1/works/work-1"));
        assert_eq!(
            work_session_binding("session-1").as_deref(),
            Some("/v1/works/session-bindings/session-1")
        );
        assert_eq!(
            work_branch_attachments("work-1", "branch.main").as_deref(),
            Some("/v1/works/work-1/branches/branch.main/attachments")
        );
        assert_eq!(
            work_branch_turns("work-1", "branch.main").as_deref(),
            Some("/v1/works/work-1/branches/branch.main/turns")
        );
        assert_eq!(
            work_branch_task_graph("work-1", "branch.main").as_deref(),
            Some("/v1/works/work-1/branches/branch.main/task-graph")
        );
        assert_eq!(
            work_branch_attachment("work-1", "branch.main", "attachment-1").as_deref(),
            Some("/v1/works/work-1/branches/branch.main/attachments/attachment-1")
        );
        assert_eq!(
            work_branch_control_operations("work-1", "branch.main").as_deref(),
            Some("/v1/works/work-1/branches/branch.main/control-operations")
        );
        for unsafe_id in ["", ".", "..", "work/other", "work?other", "work%2Fother"] {
            assert!(work(unsafe_id).is_none());
            assert!(work_session_binding(unsafe_id).is_none());
            assert!(work_branch_attachments("work-1", unsafe_id).is_none());
            assert!(work_branch_turns(unsafe_id, "branch-1").is_none());
            assert!(work_branch_task_graph("work-1", unsafe_id).is_none());
            assert!(work_branch_attachment("work-1", "branch-1", unsafe_id).is_none());
            assert!(work_branch_control_operations("work-1", unsafe_id).is_none());
        }
    }
}
