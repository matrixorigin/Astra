//! Canonical paths for the thin client protocol (§5.5 + `router_builder`).

/// `POST` — chat turn as SSE (`data: {json}\\n\\n` per event).
pub const CHAT_STREAM: &str = "/chat/stream";

/// `POST` — low-level chat turn bridge (same SSE framing as stream).
pub const CHAT_TURN: &str = "/chat/turn";

/// `POST` — design doc: edge posts tool execution results (optional route).
pub const TOOLS_RESULT: &str = "/tools/result";

/// `POST` — design doc: user approval for gated tools.
pub const APPROVAL_RESPOND: &str = "/approval/respond";

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

#[inline]
pub fn chat_session_reflect(session_id: &str) -> String {
    format!("/chat/session/{session_id}/reflect")
}

#[inline]
pub fn chat_session_decision_trace(session_id: &str) -> String {
    format!("/chat/session/{session_id}/decision-trace")
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

// ── Admin API (`mo-admin-cli` → same server) ───────────────────────────────

pub const ADMIN_INIT: &str = "/admin/init";
pub const ADMIN_AUDIT: &str = "/admin/audit";
pub const ADMIN_USERS_GRANT_ROLE: &str = "/admin/users/grant-role";
pub const ADMIN_USERS_REVOKE_ROLE: &str = "/admin/users/revoke-role";
pub const ADMIN_TOKENS: &str = "/admin/tokens";
pub const ADMIN_PROMPTS_OPTIMIZE: &str = "/admin/prompts/optimize";
pub const ADMIN_FEEDBACK_STATS: &str = "/admin/feedback/stats";
pub const ADMIN_FEEDBACK_EXPORT: &str = "/admin/feedback/export";

#[inline]
pub fn model_check(model_name: &str) -> String {
    format!("/models/{model_name}/check")
}

#[inline]
pub fn skill_versions(skill_name: &str) -> String {
    format!("/skills/{skill_name}/versions")
}
