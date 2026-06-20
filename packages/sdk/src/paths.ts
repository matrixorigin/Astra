/**
 * Canonical HTTP paths for the Astra runtime thin-client API.
 * Keep in sync with `rust/crates/astra-thin-client/src/paths.rs`.
 */

export const PATH_AUTH_REGISTER = '/auth/register';
export const PATH_AUTH_LOGIN = '/auth/login';
export const PATH_AUTH_REFRESH = '/auth/refresh';
export const PATH_AUTH_LOGOUT = '/auth/logout';
export const PATH_AUTH_ME = '/auth/me';

export const PATH_SESSIONS = '/sessions';

export const PATH_CHAT = '/chat';
export const PATH_CHAT_STREAM = '/chat/stream';

export const PATH_MODELS = '/models';
export const PATH_AGENT_BINDINGS = '/agent-bindings';
export const PATH_MODEL_GATEWAYS = '/model-gateways';

export const PATH_MEMORY_STORE = '/memory/store';
export const PATH_MEMORY_SEARCH = '/memory/search';
export const PATH_MEMORY_RETRIEVE = '/memory/retrieve';
export const PATH_MEMORY_PURGE = '/memory/purge';

export const PATH_SKILLS = '/skills';
export const PATH_SKILLS_PUBLISH = '/skills/publish';

export const PATH_RUNS = '/runs';

export const PATH_EVENTS = '/events';

export const PATH_EDGES_STATUS = '/edges/status';

export const PATH_TOOLS_RESULT = '/tools/result';
export const PATH_APPROVAL_RESPOND = '/approval/respond';
export const PATH_AGENTS_EDGE = '/agents/edge';
export const PATH_AGENTS_EDGE_HEARTBEAT = '/agents/edge/heartbeat';

/** Join optional gateway prefix (e.g. `/api`) with an API path that starts with `/`. */
export function joinApiPath(pathPrefix: string | undefined, path: string): string {
  const p = (pathPrefix ?? '').replace(/\/$/, '');
  const x = path.startsWith('/') ? path : `/${path}`;
  return p ? `${p}${x}` : x;
}

export function sessionPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}`;
}

export function sessionAuditSummaryPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/audit/summary`;
}

export function sessionClosePath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/close`;
}

export function sessionResumePath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/resume`;
}

export function sessionCancelPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/cancel`;
}

export function sessionActivityPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/activity`;
}

export function sessionStatePath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/state`;
}

export function sessionTranscriptPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/transcript`;
}

export function sessionArtifactsPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/artifacts`;
}

function isSafePathSegment(value: string): boolean {
  return (
    value.length > 0 &&
    value !== '.' &&
    value !== '..' &&
    /^[A-Za-z0-9._-]+$/.test(value)
  );
}

/**
 * Runtime route for the latest artifact of a kind.
 *
 * Artifact kind is a path segment in the Rust runtime. To keep the TS SDK and
 * Rust ThinClient behavior aligned, path-unsafe values return `null` instead
 * of being percent-encoded into a different segment meaning.
 */
export function sessionArtifactLatestPath(
  sessionId: string,
  artifactKind: string,
): string | null {
  if (!isSafePathSegment(artifactKind)) return null;
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/artifacts/latest/${artifactKind}`;
}

export function sessionArtifactPath(sessionId: string, artifactId: string): string | null {
  if (!isSafePathSegment(artifactId)) return null;
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/artifacts/${artifactId}`;
}

export function sessionArtifactDownloadPath(sessionId: string, artifactId: string): string | null {
  if (!isSafePathSegment(artifactId)) return null;
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/artifacts/${artifactId}/download`;
}

export function chatSessionReflectPath(sessionId: string): string {
  return `/chat/session/${encodeURIComponent(sessionId)}/reflect`;
}

export function chatSessionDecisionTracePath(sessionId: string): string {
  return `/chat/session/${encodeURIComponent(sessionId)}/decision-trace`;
}

export function chatRunPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}`;
}

export function chatRunStreamPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/stream`;
}

export function chatRunProjectionPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/projection`;
}

export function chatRunInputPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/input`;
}

export function chatRunPausePath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/pause`;
}

export function chatRunResumePath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/resume`;
}

export function chatRunDelegatePath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/delegate`;
}

export function chatRunDelegationsPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/delegations`;
}

export function chatRunDelegationsPausePath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/delegations/pause`;
}

export function chatRunDelegationsResumePath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/delegations/resume`;
}

export function eventsSessionPath(sessionId: string): string {
  return `/events/session/${encodeURIComponent(sessionId)}`;
}

export function eventsCausalChainPath(causalChainId: string): string {
  return `/events/causal-chain/${encodeURIComponent(causalChainId)}`;
}

export function skillPath(skillId: string): string {
  return `${PATH_SKILLS}/${encodeURIComponent(skillId)}`;
}

export function skillUnpublishPath(skillName: string): string {
  return `${PATH_SKILLS}/${encodeURIComponent(skillName)}/unpublish`;
}

export function modelPath(modelName: string): string {
  return `${PATH_MODELS}/${encodeURIComponent(modelName)}`;
}

export function modelCheckPath(modelName: string): string {
  return `${PATH_MODELS}/${encodeURIComponent(modelName)}/check`;
}

export function agentBindingPath(agentBindingId: string): string {
  return `${PATH_AGENT_BINDINGS}/${encodeURIComponent(agentBindingId)}`;
}

export function agentBindingDisablePath(agentBindingId: string): string {
  return `${agentBindingPath(agentBindingId)}/disable`;
}

export function modelGatewayPath(modelGatewayId: string): string {
  return `${PATH_MODEL_GATEWAYS}/${encodeURIComponent(modelGatewayId)}`;
}

export function modelGatewayDisablePath(modelGatewayId: string): string {
  return `${modelGatewayPath(modelGatewayId)}/disable`;
}

/** Build `?a=1&b=2` from plain values (skips undefined / null). */
export function buildQueryString(params: Record<string, string | number | undefined | null>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null) continue;
    q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : '';
}

export function taskLeasePath(taskId: string): string {
  return `/tasks/${encodeURIComponent(taskId)}/lease`;
}

export function taskLeaseClaimPath(taskId: string): string {
  return `/tasks/${encodeURIComponent(taskId)}/lease/claim`;
}

export function taskLeaseReleasePath(taskId: string): string {
  return `/tasks/${encodeURIComponent(taskId)}/lease/release`;
}

export function taskLeaseRenewPath(taskId: string): string {
  return `/tasks/${encodeURIComponent(taskId)}/lease/renew`;
}

/** HTTP header for edge transport instance id (matches Rust `ASTRA_EDGE_ID_HEADER`). */
export const ASTRA_EDGE_ID_HEADER = 'X-Astra-Edge-Id';
