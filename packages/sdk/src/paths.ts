/**
 * Canonical HTTP paths for the Astra runtime thin-client API.
 * Keep in sync with `crates/astra-thin-client/src/paths.rs`.
 */

export const PATH_AUTH_REGISTER = "/auth/register";
export const PATH_AUTH_LOGIN = "/auth/login";
export const PATH_AUTH_REFRESH = "/auth/refresh";
export const PATH_AUTH_LOGOUT = "/auth/logout";
export const PATH_AUTH_ME = "/auth/me";
export const PATH_AUTH_REAUTHENTICATE = "/auth/reauthenticate";

export const PATH_SESSIONS = "/sessions";

export const PATH_CHAT = "/chat";
export const PATH_CHAT_STREAM = "/chat/stream";

export const PATH_MODELS = "/models";
export const PATH_MODEL_ACCESS = "/model-access";
export const PATH_AGENT_BINDINGS = "/agent-bindings";

export const PATH_MEMORY_STORE = "/memory/store";
export const PATH_MEMORY_SEARCH = "/memory/search";
export const PATH_MEMORY_RETRIEVE = "/memory/retrieve";
export const PATH_MEMORY_PURGE = "/memory/purge";

export const PATH_SKILLS = "/skills";
export const PATH_SKILLS_PUBLISH = "/skills/publish";

export const PATH_RUNS = "/runs";
export const PATH_WORKS = "/v1/works";
export const ASTRA_WORK_API_MAJOR_HEADER = "x-astra-work-api-major";
export const ASTRA_WORK_API_MAJOR = "1";

export const PATH_EVENTS = "/events";

export const PATH_EDGES_STATUS = "/edges/status";
export const PATH_RUNTIME_CAPABILITIES = "/runtime/capabilities";

export const PATH_TOOLS_RESULT = "/tools/result";
export const PATH_APPROVAL_RESPOND = "/approval/respond";
export const PATH_AGENTS_EDGE = "/agents/edge";
export const PATH_AGENTS_EDGE_HEARTBEAT = "/agents/edge/heartbeat";

/** Join optional gateway prefix (e.g. `/api`) with an API path that starts with `/`. */
export function joinApiPath(
  pathPrefix: string | undefined,
  path: string,
): string {
  const p = (pathPrefix ?? "").replace(/\/$/, "");
  const x = path.startsWith("/") ? path : `/${path}`;
  return p ? `${p}${x}` : x;
}

export function sessionPath(sessionId: string): string {
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}`;
}

export function workPath(workId: string): string {
  if (
    workId === "." ||
    workId === ".." ||
    workId.length === 0 ||
    Array.from(workId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(workId)
  ) {
    throw new TypeError("workId is not a canonical Work resource identity");
  }
  return `${PATH_WORKS}/${encodeURIComponent(workId)}`;
}

export function workSessionBindingPath(sessionId: string): string {
  if (
    sessionId === "." ||
    sessionId === ".." ||
    sessionId.length === 0 ||
    Array.from(sessionId).length > 128 ||
    !/^[A-Za-z0-9._-]+$/u.test(sessionId)
  ) {
    throw new TypeError("sessionId is not a canonical Work binding identity");
  }
  return `${PATH_WORKS}/session-bindings/${encodeURIComponent(sessionId)}`;
}

export function workReadCursorPath(workId: string): string {
  return `${workPath(workId)}/read-cursor`;
}

export function workEventsPath(workId: string): string {
  return `${workPath(workId)}/events`;
}

export function workBranchesPath(workId: string): string {
  return `${workPath(workId)}/branches`;
}

export function workArchivedBranchesPath(workId: string): string {
  return `${workBranchesPath(workId)}/archived`;
}

export function workBranchComparisonsPath(workId: string): string {
  return `${workPath(workId)}/branch-comparisons`;
}

export function workActionsPath(workId: string): string {
  return `${workPath(workId)}/actions`;
}

export function workCriteriaPath(workId: string): string {
  return `${workPath(workId)}/criteria`;
}

function workBranchPath(
  workId: string,
  branchId: string,
): string {
  if (
    branchId === "." ||
    branchId === ".." ||
    branchId.length === 0 ||
    Array.from(branchId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(branchId)
  ) {
    throw new TypeError("branchId is not a canonical Work resource identity");
  }
  return `${workPath(workId)}/branches/${encodeURIComponent(branchId)}`;
}

export function workBranchTurnsPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/turns`;
}

export function workBranchActionsPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/actions`;
}

export function workBranchPatchArtifactPath(
  workId: string,
  branchId: string,
  patchArtifactId: string,
): string {
  if (
    patchArtifactId === "." ||
    patchArtifactId === ".." ||
    patchArtifactId.length === 0 ||
    Array.from(patchArtifactId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(patchArtifactId)
  ) {
    throw new TypeError("patchArtifactId is not a canonical Work resource identity");
  }
  return `${workBranchPath(workId, branchId)}/patch-artifacts/${encodeURIComponent(patchArtifactId)}`;
}

export function workBranchPatchArtifactsPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/patch-artifacts`;
}

export function workBranchPatchArtifactContentPath(
  workId: string,
  branchId: string,
  patchArtifactId: string,
): string {
  return `${workBranchPatchArtifactPath(workId, branchId, patchArtifactId)}/content`;
}

export function workPatchMaterializationsPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/patch-materializations`;
}

export function workPatchMaterializationPath(
  workId: string,
  branchId: string,
  operationId: string,
): string {
  if (
    operationId === "." ||
    operationId === ".." ||
    operationId.length === 0 ||
    Array.from(operationId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(operationId)
  ) {
    throw new TypeError("operationId is not a canonical Work resource identity");
  }
  return `${workPatchMaterializationsPath(workId, branchId)}/${encodeURIComponent(operationId)}`;
}

export function workPatchCommitsPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/patch-commits`;
}

export function workPatchCommitPath(
  workId: string,
  branchId: string,
  operationId: string,
): string {
  if (
    operationId === "." ||
    operationId === ".." ||
    operationId.length === 0 ||
    Array.from(operationId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(operationId)
  ) {
    throw new TypeError("operationId is not a canonical Work resource identity");
  }
  return `${workPatchCommitsPath(workId, branchId)}/${encodeURIComponent(operationId)}`;
}

export function workBranchTranscriptPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/transcript`;
}

export function workBranchAttachmentsPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/attachments`;
}

export function workBranchAttachmentPath(
  workId: string,
  branchId: string,
  attachmentId: string,
): string {
  if (
    attachmentId === "." ||
    attachmentId === ".." ||
    attachmentId.length === 0 ||
    Array.from(attachmentId).length > 128 ||
    !/^[A-Za-z0-9._:-]+$/u.test(attachmentId)
  ) {
    throw new TypeError("attachmentId is not a canonical Work resource identity");
  }
  return `${workBranchAttachmentsPath(workId, branchId)}/${encodeURIComponent(attachmentId)}`;
}

export function workBranchControlOperationsPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/control-operations`;
}

export function workBranchControlOperationPath(
  workId: string,
  branchId: string,
  operationId: string,
): string {
  if (
    operationId === "." ||
    operationId === ".." ||
    operationId.length === 0 ||
    Array.from(operationId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(operationId)
  ) {
    throw new TypeError("operationId is not a canonical Work resource identity");
  }
  return `${workBranchControlOperationsPath(workId, branchId)}/${encodeURIComponent(operationId)}`;
}

export function workBranchForksPath(workId: string, branchId: string): string {
  return `${workBranchPath(workId, branchId)}/forks`;
}

export function workBranchForkPath(
  workId: string,
  branchId: string,
  operationId: string,
): string {
  if (
    operationId === "." ||
    operationId === ".." ||
    operationId.length === 0 ||
    Array.from(operationId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(operationId)
  ) {
    throw new TypeError("operationId is not a canonical Work resource identity");
  }
  return `${workBranchForksPath(workId, branchId)}/${encodeURIComponent(operationId)}`;
}

export function workBranchDeletionOperationsPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/deletion-operations`;
}

export function workBranchDeletionOperationPath(
  workId: string,
  branchId: string,
  operationId: string,
): string {
  if (
    operationId === "." ||
    operationId === ".." ||
    operationId.length === 0 ||
    Array.from(operationId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(operationId)
  ) {
    throw new TypeError("operationId is not a canonical Work resource identity");
  }
  return `${workBranchDeletionOperationsPath(workId, branchId)}/${encodeURIComponent(operationId)}`;
}

export function workBranchTaskGraphPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/task-graph`;
}

export function workBranchCriteriaProposalsPath(
  workId: string,
  branchId: string,
): string {
  return `${workBranchPath(workId, branchId)}/criteria-proposals`;
}

export function workBranchCriteriaProposalPath(
  workId: string,
  branchId: string,
  proposalId: string,
): string {
  if (
    proposalId === "." ||
    proposalId === ".." ||
    proposalId.length === 0 ||
    Array.from(proposalId).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(proposalId)
  ) {
    throw new TypeError("proposalId is not a canonical Work resource identity");
  }
  return `${workBranchCriteriaProposalsPath(workId, branchId)}/${encodeURIComponent(proposalId)}`;
}

export function workBranchCriteriaProposalDecisionPath(
  workId: string,
  branchId: string,
  proposalId: string,
): string {
  return `${workBranchCriteriaProposalPath(workId, branchId, proposalId)}/decision`;
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
    value !== "." &&
    value !== ".." &&
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

export function sessionArtifactPath(
  sessionId: string,
  artifactId: string,
): string | null {
  if (!isSafePathSegment(artifactId)) return null;
  return `${PATH_SESSIONS}/${encodeURIComponent(sessionId)}/artifacts/${artifactId}`;
}

export function sessionArtifactDownloadPath(
  sessionId: string,
  artifactId: string,
): string | null {
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

export function chatRunProjectionRepairPath(runId: string): string {
  return `/chat/runs/${encodeURIComponent(runId)}/projection/repair`;
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

/** Build `?a=1&b=2` from plain values (skips undefined / null). */
export function buildQueryString(
  params: Record<string, string | number | undefined | null>,
): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v === undefined || v === null) continue;
    q.set(k, String(v));
  }
  const s = q.toString();
  return s ? `?${s}` : "";
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
export const ASTRA_EDGE_ID_HEADER = "X-Astra-Edge-Id";
