import type {
  AstraClientConfig,
  AgentBindingCreateRequest,
  AgentBindingCreateResponse,
  AgentBindingRecord,
  ApprovalRespondRequestBody,
  AuthResult,
  ChatRequest,
  DelegationListResponse,
  DelegationMutationResponse,
  DelegationRequestBody,
  DelegationResponse,
  EdgeHeartbeatRequestBody,
  EdgeRegisterRequestBody,
  EdgeStatusResponse,
  EventListCursor,
  EventListFilters,
  EventListResponse,
  EventResponse,
  MemoryEntry,
  MemorySearchResult,
  ReflectQueryParams,
  ReflectReport,
  RegisterSkillBody,
  PublishSkillBody,
  RunInputRequestBody,
  RunInputResponse,
  RunListCursor,
  RunListParams,
  RunListResponse,
  RunProjectionRepairResponse,
  RunProjectionResponse,
  RunStatus,
  RuntimeChatResponse,
  RuntimeArtifactListParams,
  RuntimeArtifactListResponse,
  RuntimeModelListItem,
  RuntimeModelCatalogCursor,
  RuntimeModelListPageResponse,
  RuntimeModelAccessProjection,
  RuntimeSessionCreateBody,
  RuntimeSessionListParams,
  RuntimeSkillListResponse,
  RuntimeSkillListParams,
  RuntimeSessionListResponse,
  RuntimeSessionResponse,
  RuntimeSessionUpdateBody,
  RuntimeTranscriptParams,
  RuntimeTranscriptResponse,
  ReauthenticationProof,
  ReauthenticationPurpose,
  SessionActivityCursor,
  SessionActivityResponse,
  SessionAuditSummary,
  SessionInfo,
  SessionUpdateBody,
  SkillInfo,
  SkillRecord,
  StreamEvent,
  ConnectionState,
  TaskLeaseMutationRequestBody,
  ToolResultRequestBody,
  UserInfo,
  WorkCreateInput,
  WorkCatalogCursorV1,
  WorkCatalogPageV1,
  WorkBranchAttachmentV1,
  WorkArchivedBranchListParamsV1,
  WorkArchivedBranchPageV1,
  WorkBranchControlBasisV1,
  WorkBranchControlCommand,
  WorkBranchControlOperationV2,
  WorkBranchCreationInputV1,
  WorkBranchCreationOperationV1,
  WorkBranchDeletionInputV1,
  WorkBranchDeletionOperationV1,
  WorkBranchRetentionInputV1,
  WorkBranchRetentionReceiptV1,
  WorkBranchCatalogV1,
  WorkBranchComparisonReportV2,
  WorkContentHash,
  WorkPatchArtifactV1,
  WorkPatchArtifactContent,
  WorkPatchArtifactListParamsV1,
  WorkPatchArtifactPageV1,
  WorkPatchArtifactExportInputV1,
  WorkPatchMaterializationInputV1,
  WorkPatchMaterializationListParamsV1,
  WorkPatchMaterializationOperationV2,
  WorkPatchMaterializationPageV2,
  WorkPatchCommitInputV1,
  WorkPatchCommitListParamsV1,
  WorkPatchCommitOperationV1,
  WorkPatchCommitPageV1,
  WorkDeliverySelectionInputV1,
  WorkDeliverySelectionReceiptV1,
  WorkTranscriptPageV1,
  WorkCriteriaCursorV1,
  WorkCriteriaPageV1,
  WorkCriteriaProposalDecisionInput,
  WorkCriteriaProposalDetailV1,
  WorkCriteriaProposalListV1,
  WorkCriteriaProposalSummaryV1,
  WorkObservationReportV1,
  WorkEventPageV1,
  WorkReadCursorReceiptV1,
  WorkTaskGraphCursorV1,
  WorkTaskGraphPageV2,
  WorkSessionBindingV1,
  WorkTurnInput,
  WorkTurnStreamEvent,
} from "./types";
import {
  ASTRA_EDGE_ID_HEADER,
  ASTRA_WORK_API_MAJOR,
  ASTRA_WORK_API_MAJOR_HEADER,
  PATH_AGENT_BINDINGS,
  PATH_AGENTS_EDGE,
  PATH_AGENTS_EDGE_HEARTBEAT,
  PATH_APPROVAL_RESPOND,
  PATH_AUTH_LOGIN,
  PATH_AUTH_LOGOUT,
  PATH_AUTH_ME,
  PATH_AUTH_REFRESH,
  PATH_AUTH_REAUTHENTICATE,
  PATH_AUTH_REGISTER,
  PATH_CHAT,
  PATH_CHAT_STREAM,
  PATH_EDGES_STATUS,
  PATH_EVENTS,
  PATH_MEMORY_PURGE,
  PATH_MEMORY_RETRIEVE,
  PATH_MEMORY_SEARCH,
  PATH_MEMORY_STORE,
  PATH_MODELS,
  PATH_MODEL_ACCESS,
  PATH_RUNS,
  PATH_SESSIONS,
  PATH_SKILLS,
  PATH_SKILLS_PUBLISH,
  PATH_TOOLS_RESULT,
  PATH_WORKS,
  agentBindingDisablePath,
  agentBindingPath,
  buildQueryString,
  chatRunDelegatePath,
  chatRunDelegationsPath,
  chatRunDelegationsPausePath,
  chatRunDelegationsResumePath,
  chatRunInputPath,
  chatRunPath,
  chatRunPausePath,
  chatRunProjectionPath,
  chatRunProjectionRepairPath,
  chatRunResumePath,
  chatRunStreamPath,
  chatSessionDecisionTracePath,
  chatSessionReflectPath,
  eventsCausalChainPath,
  eventsSessionPath,
  joinApiPath,
  sessionActivityPath,
  sessionArtifactsPath,
  sessionAuditSummaryPath,
  sessionCancelPath,
  sessionClosePath,
  sessionPath,
  sessionResumePath,
  sessionTranscriptPath,
  skillPath,
  skillUnpublishPath,
  taskLeaseClaimPath,
  taskLeasePath,
  taskLeaseReleasePath,
  taskLeaseRenewPath,
  workPath,
  workArchivedBranchesPath,
  workBranchesPath,
  workBranchComparisonsPath,
  workActionsPath,
  workCriteriaPath,
  workEventsPath,
  workBranchTurnsPath,
  workBranchActionsPath,
  workBranchTranscriptPath,
  workBranchAttachmentPath,
  workBranchPatchArtifactPath,
  workBranchPatchArtifactContentPath,
  workBranchPatchArtifactsPath,
  workBranchAttachmentsPath,
  workBranchControlOperationPath,
  workBranchControlOperationsPath,
  workBranchForkPath,
  workBranchForksPath,
  workBranchDeletionOperationPath,
  workBranchDeletionOperationsPath,
  workBranchTaskGraphPath,
  workSessionBindingPath,
  workBranchCriteriaProposalsPath,
  workBranchCriteriaProposalPath,
  workBranchCriteriaProposalDecisionPath,
  workReadCursorPath,
  workPatchMaterializationPath,
  workPatchMaterializationsPath,
  workPatchCommitPath,
  workPatchCommitsPath,
} from "./paths";
import { SSEClient, parseSseDataEvents } from "./sse-client";
import {
  headersInitToRecord,
  readAstraError,
  readAstraErrorDetail,
} from "./http";
import { modelSelectionToWire } from "./wire";
import {
  decodeWorkObservationReportV1,
  decodeWorkArchivedBranchPageV1,
  decodeWorkCatalogPageV1,
  decodeWorkBranchAttachmentV1,
  decodeWorkBranchControlOperationV2,
  decodeWorkBranchCreationOperationV1,
  decodeWorkBranchDeletionOperationV1,
  decodeWorkBranchRetentionReceiptV1,
  decodeWorkBranchCatalogV1,
  decodeWorkBranchComparisonReportV2,
  decodeWorkPatchArtifactV1,
  decodeWorkPatchArtifactPageV1,
  decodeWorkPatchMaterializationOperationV2,
  decodeWorkPatchMaterializationPageV2,
  decodeWorkPatchCommitOperationV1,
  decodeWorkPatchCommitPageV1,
  decodeWorkDeliverySelectionReceiptV1,
  decodeWorkConversationHeadV1,
  decodeWorkTranscriptPageV1,
  decodeWorkCriteriaPageV1,
  decodeWorkCriteriaProposalDetailV1,
  decodeWorkCriteriaProposalListV1,
  decodeWorkCriteriaProposalSummaryV1,
  decodeWorkEventPageV1,
  decodeWorkReadCursorReceiptV1,
  decodeWorkTaskGraphPageV2,
  decodeWorkSessionBindingV1,
  decodeWorkTurnStreamEventV1,
} from "./work-wire";

type SessionWire = RuntimeSessionResponse;
type SessionListWire = RuntimeSessionListResponse;

function assertWorkRequestId(value: string): void {
  const bytes = new TextEncoder().encode(value).length;
  if (bytes === 0 || bytes > 256 || /[\u0000-\u001f\u007f-\u009f]/u.test(value)) {
    throw new TypeError(
      "requestId must be non-empty, control-free, and at most 256 UTF-8 bytes",
    );
  }
}

function assertWorkCommitMessage(value: string): void {
  const bytes = new TextEncoder().encode(value).length;
  if (
    value.trim().length === 0 ||
    bytes > 4_096 ||
    /[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/u.test(value)
  ) {
    throw new TypeError(
      "message must be non-empty, control-free, and at most 4096 UTF-8 bytes",
    );
  }
}

type RunStatusWire = {
  run_id: string;
  session_id: string;
  parent_run_id?: string | null;
  root_run_id?: string | null;
  depth?: number;
  status: string;
  waiting_for?: string | null;
  events_count: number;
  workspace?: RunStatus["workspace"];
  executor?: RunStatus["executor"];
  transport?: string | null;
  fallback_policy?: string | null;
};

type RunListWire = {
  runs: RunStatusWire[];
  total?: number | null;
  limit: number;
  next_cursor?: RunListCursorWire | null;
};

type RunListCursorWire = {
  updated_at: string;
  run_id: string;
};

type RunInputResponseWire = {
  run_id: string;
  accepted: boolean;
  duplicate: boolean;
};

type ChatResponseWire = RuntimeChatResponse;

function normalizeSession(w: SessionWire): SessionInfo {
  return {
    sessionId: w.session_id,
    userId: w.user_id,
    agentId: w.agent_id ?? undefined,
    title: w.title ?? undefined,
    status: w.status,
    createdAt: w.created_at,
    lastActive: w.updated_at ?? w.ended_at ?? w.created_at,
  };
}

function normalizeRunStatus(w: RunStatusWire): RunStatus {
  return {
    runId: w.run_id,
    sessionId: w.session_id,
    parentRunId: w.parent_run_id ?? null,
    rootRunId: w.root_run_id ?? w.run_id,
    depth: Number.isSafeInteger(w.depth) && Number(w.depth) >= 0 ? Number(w.depth) : 0,
    status: w.status as RunStatus["status"],
    eventsCount: Number(w.events_count),
    waitingFor: w.waiting_for ?? undefined,
    workspace: w.workspace,
    executor: w.executor,
    transport: w.transport ?? undefined,
    fallbackPolicy: w.fallback_policy ?? undefined,
  };
}

function normalizeRunList(w: RunListWire): RunListResponse {
  return {
    runs: w.runs.map(normalizeRunStatus),
    total: w.total ?? null,
    limit: w.limit,
    nextCursor: normalizeRunListCursor(w.next_cursor),
  };
}

function normalizeRunListCursor(
  cursor: RunListCursorWire | null | undefined,
): RunListCursor | null {
  if (!cursor) return null;
  return {
    updatedAt: cursor.updated_at,
    runId: cursor.run_id,
  };
}

function normalizeRunInputResponse(w: RunInputResponseWire): RunInputResponse {
  return {
    runId: w.run_id,
    accepted: Boolean(w.accepted),
    duplicate: Boolean(w.duplicate),
  };
}

function normalizeEventList(raw: EventListResponse): EventListResponse {
  return {
    ...raw,
    events: raw.events.map((ev) => ({
      ...ev,
      metadata:
        ev.metadata &&
        typeof ev.metadata === "object" &&
        !Array.isArray(ev.metadata)
          ? (ev.metadata as Record<string, unknown>)
          : {},
    })),
  };
}

function normalizeReflectReport(
  raw: unknown,
  fallback: { sessionId: string; focus: string },
): ReflectReport {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error("Astra returned an invalid reflection report.");
  }
  const report = raw as Record<string, unknown>;
  const overview =
    report.overview &&
    typeof report.overview === "object" &&
    !Array.isArray(report.overview)
      ? (report.overview as Record<string, unknown>)
      : {};
  return {
    session_id:
      typeof report.session_id === "string" && report.session_id.trim()
        ? report.session_id
        : fallback.sessionId,
    focus:
      typeof report.focus === "string" && report.focus.trim()
        ? report.focus
        : fallback.focus,
    overview,
    diagnoses: Array.isArray(report.diagnoses) ? report.diagnoses : [],
    insights: Array.isArray(report.insights) ? report.insights : [],
    recommendations: Array.isArray(report.recommendations)
      ? report.recommendations.filter(
          (value): value is string =>
            typeof value === "string" && value.trim().length > 0,
        )
      : [],
    ...(report.reflection_context !== undefined
      ? { reflection_context: report.reflection_context }
      : {}),
    ...(typeof report.prompt_preview === "string" ||
    report.prompt_preview === null
      ? { prompt_preview: report.prompt_preview }
      : {}),
    ...(report.evidence_graph !== undefined
      ? { evidence_graph: report.evidence_graph }
      : {}),
  };
}

export function chatRequestToWire(req: ChatRequest): Record<string, unknown> {
  const body: Record<string, unknown> = {
    message: req.message,
    model_selection: modelSelectionToWire(req.modelSelection),
  };
  if (req.parts) body.parts = req.parts;
  if (req.attachments) body.attachments = req.attachments;
  if (req.executionBudget) {
    body.execution_budget = {
      initial_turns: req.executionBudget.initialTurns,
      hard_turn_limit: req.executionBudget.hardTurnLimit,
    };
  }
  if (req.sessionId) body.session_id = req.sessionId;
  if (req.workBinding) {
    body.work_binding = {
      work_id: req.workBinding.workId,
      branch_id: req.workBinding.branchId,
    };
  }
  if (req.agentId) body.agent_id = req.agentId;
  if (req.agentBinding) {
    body.agent_binding = {
      id: req.agentBinding.id,
      capability_server_refs: {
        mcp: req.agentBinding.capabilityServerRefs.mcp,
        skills: req.agentBinding.capabilityServerRefs.skills,
      },
    };
  }
  if (req.runtimeAuth) {
    body.runtime_auth = {
      authorization: req.runtimeAuth.authorization,
    };
  }
  if (req.runtimeProfile) body.runtime_profile = req.runtimeProfile;
  if (req.context) body.context = req.context;
  if (req.explain !== undefined) body.explain = req.explain;
  if (req.planSubtaskId) body.plan_subtask_id = req.planSubtaskId;
  if (req.isPlanSubtask !== undefined) body.is_plan_subtask = req.isPlanSubtask;
  if (req.edgeExecutorId) body.edge_executor_id = req.edgeExecutorId;
  if (req.capabilities?.length) body.capabilities = req.capabilities;
  if (req.allowSkills?.length) body.allow_skills = req.allowSkills;
  if (req.allowTools?.length) body.allow_tools = req.allowTools;
  if (req.enabledTools !== undefined) body.enabled_tools = req.enabledTools;
  if (req.workspaceBinding) body.workspace_binding = req.workspaceBinding;
  if (req.executorBinding) body.executor_binding = req.executorBinding;
  if (req.skillSearch) {
    body.skill_search = {
      dynamic_surface: req.skillSearch.dynamicSurface,
      min_catalog_size: req.skillSearch.minCatalogSize,
      surface_cap: req.skillSearch.surfaceCap,
    };
  }
  return body;
}

/**
 * Astra HTTP + SSE client for server communication.
 *
 * Handles authentication (JWT with auto-refresh), REST endpoints for
 * sessions/runs, and SSE streaming for chat responses. Paths default to the
 * same layout as `astra-thin-client` / `astra-server`; set `pathPrefix` when a
 * gateway mounts the API under a prefix.
 */
export class AstraClient {
  private config: AstraClientConfig;
  private accessToken: string | null;
  private refreshTokenValue: string | null;

  constructor(config: AstraClientConfig) {
    this.config = config;
    this.accessToken = config.accessToken ?? null;
    this.refreshTokenValue = config.refreshToken ?? null;
  }

  private apiPath(path: string): string {
    const base = this.config.baseUrl.replace(/\/$/, "");
    return `${base}${joinApiPath(this.config.pathPrefix, path)}`;
  }

  // ─── Auth ──────────────────────────────────────────────────────────

  /**
   * Register a new user account. The server requires `email`; if omitted,
   * a placeholder `{username}@users.local.astra` is sent.
   */
  async register(
    username: string,
    password: string,
    options?: { email?: string; displayName?: string },
  ): Promise<AuthResult> {
    const email = options?.email ?? `${username}@users.local.astra`;
    const body: Record<string, unknown> = { username, email, password };
    if (options?.displayName) body.display_name = options.displayName;
    const result = await this.post<AuthResult>(PATH_AUTH_REGISTER, body);
    this.accessToken = result.access_token;
    this.refreshTokenValue = result.refresh_token;
    return result;
  }

  /** Log in with username/password. Stores tokens automatically. */
  async login(username: string, password: string): Promise<AuthResult> {
    const result = await this.post<AuthResult>(PATH_AUTH_LOGIN, {
      username,
      password,
    });
    this.accessToken = result.access_token;
    this.refreshTokenValue = result.refresh_token;
    return result;
  }

  /** Log out and clear stored tokens. Requires refresh token in client state. */
  async logout(): Promise<void> {
    try {
      if (this.refreshTokenValue) {
        await this.post(PATH_AUTH_LOGOUT, {
          refresh_token: this.refreshTokenValue,
        });
      }
    } finally {
      this.accessToken = null;
      this.refreshTokenValue = null;
    }
  }

  /** Get the current authenticated user's info. */
  async getMe(): Promise<UserInfo> {
    return this.fetch<UserInfo>(PATH_AUTH_ME);
  }

  async reauthenticate(
    password: string,
    purpose: ReauthenticationPurpose,
  ): Promise<ReauthenticationProof> {
    if (password.length === 0 || password.length > 4096) {
      throw new TypeError("password must be non-empty and bounded");
    }
    const raw = await this.post<unknown>(PATH_AUTH_REAUTHENTICATE, { password, purpose });
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
      throw new TypeError("reauthentication response must be an object");
    }
    const object = raw as Record<string, unknown>;
    if (
      Object.keys(object).sort().join("\0") !== "expires_in\0proof\0purpose" ||
      typeof object.proof !== "string" ||
      object.proof.length === 0 ||
      object.proof.length > 160 ||
      object.proof.trim() !== object.proof ||
      object.purpose !== purpose ||
      !Number.isSafeInteger(object.expires_in) ||
      Number(object.expires_in) < 1
    ) {
      throw new TypeError("reauthentication response is invalid");
    }
    return {
      proof: object.proof,
      purpose,
      expires_in: Number(object.expires_in),
    };
  }

  setTokens(accessToken: string, refreshToken?: string): void {
    this.accessToken = accessToken;
    if (refreshToken) this.refreshTokenValue = refreshToken;
  }

  private async tryRefreshToken(): Promise<boolean> {
    if (!this.refreshTokenValue) return false;
    try {
      const res = await fetch(this.apiPath(PATH_AUTH_REFRESH), {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refresh_token: this.refreshTokenValue }),
      });
      if (!res.ok) return false;
      const data = (await res.json()) as AuthResult;
      this.accessToken = data.access_token;
      this.refreshTokenValue = data.refresh_token;
      await this.config.onTokenRefresh?.({
        accessToken: data.access_token,
        refreshToken: data.refresh_token,
      });
      return true;
    } catch {
      return false;
    }
  }

  private buildHeaders(
    init?: RequestInit,
    extra?: Record<string, string>,
  ): Record<string, string> {
    const headers: Record<string, string> = {
      ...this.config.headers,
      ...extra,
    };
    const method = (init?.method ?? "GET").toUpperCase();
    if (
      init?.body != null &&
      (method === "POST" ||
        method === "PUT" ||
        method === "PATCH" ||
        method === "DELETE")
    ) {
      headers["Content-Type"] = "application/json";
    }
    if (this.accessToken) {
      headers["Authorization"] = `Bearer ${this.accessToken}`;
    }
    return headers;
  }

  // ─── HTTP helpers ──────────────────────────────────────────────────

  private async request(path: string, init?: RequestInit): Promise<Response> {
    const url = this.apiPath(path);
    let res = await fetch(url, {
      ...init,
      headers: headersInitToRecord(this.buildHeaders(init), init?.headers),
    });

    if (res.status === 401) {
      const refreshed = await this.tryRefreshToken();
      if (refreshed) {
        res = await fetch(url, {
          ...init,
          headers: headersInitToRecord(this.buildHeaders(init), init?.headers),
        });
      }
    }

    if (!res.ok) {
      const error = await readAstraError(res);
      throw new AstraApiError(
        res.status,
        error.detail,
        path,
        error.code,
        error.category,
        error.retryable,
        error.actionHints,
      );
    }

    return res;
  }

  async fetch<T>(path: string, init?: RequestInit): Promise<T> {
    const res = await this.request(path, init);

    if (res.status === 204 || res.headers.get("content-length") === "0") {
      return undefined as T;
    }

    const text = await res.text();
    if (!text) return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch (parseErr) {
      const msg =
        parseErr instanceof Error ? parseErr.message : String(parseErr);
      throw new AstraApiError(
        res.status,
        `Invalid JSON response: ${msg}; body starts: ${text.slice(0, 500)}`,
        path,
      );
    }
  }

  private async fetchWorkPatchContent(path: string): Promise<WorkPatchArtifactContent> {
    const res = await this.request(path, {
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
    const contentType = res.headers.get("content-type")?.toLowerCase();
    const lengthText = res.headers.get("content-length");
    const etag = res.headers.get("etag");
    const length = lengthText == null ? Number.NaN : Number(lengthText);
    const hash = etag?.match(/^"(sha256:[0-9a-f]{64})"$/u)?.[1];
    if (
      contentType?.split(";", 1)[0]?.trim() !== "text/x-diff" ||
      !Number.isSafeInteger(length) ||
      length < 1 ||
      length > 16 * 1024 * 1024 ||
      hash == null
    ) {
      throw new TypeError("Work patch content response metadata is invalid");
    }
    const data = await res.text();
    if (new TextEncoder().encode(data).byteLength !== length) {
      throw new TypeError("Work patch content length disagrees with its response metadata");
    }
    return { data, hash: hash as WorkContentHash, bytes: length };
  }

  async post<T>(path: string, body?: unknown, init?: RequestInit): Promise<T> {
    return this.fetch<T>(path, {
      ...init,
      method: "POST",
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  }

  async put<T>(path: string, body?: unknown, init?: RequestInit): Promise<T> {
    return this.fetch<T>(path, {
      ...init,
      method: "PUT",
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  }

  // ─── Work ─────────────────────────────────────────────────────────

  /** List one bounded owner-scoped Work page in stable creation order. */
  async listWorks(
    options: { cursor?: WorkCatalogCursorV1; limit?: number } = {},
  ): Promise<WorkCatalogPageV1> {
    if (
      options.limit !== undefined &&
      (!Number.isSafeInteger(options.limit) || options.limit < 1 || options.limit > 50)
    ) {
      throw new TypeError("limit must be a safe integer between 1 and 50");
    }
    if (options.cursor !== undefined) {
      workPath(options.cursor.work_id);
      if (
        !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u.test(
          options.cursor.created_at,
        ) ||
        !Number.isFinite(Date.parse(options.cursor.created_at))
      ) {
        throw new TypeError("cursor.created_at must be an RFC 3339 UTC timestamp");
      }
    }
    const raw = await this.fetch<unknown>(
      `${PATH_WORKS}${buildQueryString({
        before_created_at: options.cursor?.created_at,
        before_work_id: options.cursor?.work_id,
        limit: options.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    return decodeWorkCatalogPageV1(raw);
  }

  /** Atomically start one Work with a server-owned branch and conversation. */
  async createWork(input: WorkCreateInput): Promise<WorkObservationReportV1> {
    assertWorkRequestId(input.requestId);
    const goalBytes = new TextEncoder().encode(input.goal).length;
    if (input.goal.trim().length === 0 || goalBytes > 8 * 1024) {
      throw new TypeError(
        "goal must be non-empty and at most 8192 UTF-8 bytes",
      );
    }
    if (!Array.isArray(input.criteria) || input.criteria.length > 128) {
      throw new TypeError("criteria must be an array with at most 128 members");
    }
    const criterionIds = new Set<string>();
    let criteriaPayloadBytes = 0;
    const criteria = input.criteria.map((criterion) => {
      if (
        criterion.criterionId.length === 0 ||
        criterion.criterionId.length > 64 ||
        criterion.criterionId === "." ||
        criterion.criterionId === ".." ||
        !/^[A-Za-z0-9._-]+$/.test(criterion.criterionId)
      ) {
        throw new TypeError(
          "criterionId must be a safe resource identity of at most 64 characters",
        );
      }
      if (criterionIds.has(criterion.criterionId)) {
        throw new TypeError(`criteria repeats criterionId ${criterion.criterionId}`);
      }
      criterionIds.add(criterion.criterionId);
      if (
        criterion.kind !== "command_check" &&
        criterion.kind !== "test_check" &&
        criterion.kind !== "human_review"
      ) {
        throw new TypeError("criterion kind is not supported by Start Work");
      }
      const statementBytes = new TextEncoder().encode(criterion.statement).length;
      if (criterion.statement.trim().length === 0 || statementBytes > 16 * 1024) {
        throw new TypeError(
          "criterion statement must be non-empty and at most 16384 UTF-8 bytes",
        );
      }
      criteriaPayloadBytes += criterion.criterionId.length + statementBytes;
      if (criterion.kind === "human_review") {
        return {
          criterion_id: criterion.criterionId,
          kind: criterion.kind,
          statement: criterion.statement,
        };
      }
      const commandBytes = new TextEncoder().encode(criterion.command).length;
      if (criterion.command.trim().length === 0 || commandBytes > 64 * 1024) {
        throw new TypeError(
          "criterion command must be non-empty and at most 65536 UTF-8 bytes",
        );
      }
      criteriaPayloadBytes += commandBytes;
      return {
        criterion_id: criterion.criterionId,
        kind: criterion.kind,
        statement: criterion.statement,
        command: criterion.command,
      };
    });
    if (criteriaPayloadBytes > 1024 * 1024) {
      throw new TypeError(
        "criterion definitions must total at most 1048576 UTF-8 bytes",
      );
    }
    criteria.sort((left, right) =>
      left.criterion_id < right.criterion_id
        ? -1
        : left.criterion_id > right.criterion_id
          ? 1
          : 0,
    );
    const raw = await this.post<unknown>(
      PATH_WORKS,
      { request_id: input.requestId, goal: input.goal, criteria },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    return decodeWorkObservationReportV1(raw);
  }

  /** Read one canonical Work without exposing its internal session identity. */
  async getWorkOverview(workId: string): Promise<WorkObservationReportV1> {
    const raw = await this.fetch<unknown>(workPath(workId), {
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
    return decodeWorkObservationReportV1(raw);
  }

  /** Read the complete bounded set of active alternatives for one Work. */
  async listWorkBranches(workId: string): Promise<WorkBranchCatalogV1> {
    const raw = await this.fetch<unknown>(workBranchesPath(workId), {
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
    const catalog = decodeWorkBranchCatalogV1(raw);
    if (catalog.work_id !== workId) {
      throw new TypeError("Work branch catalog identity disagrees with the requested Work");
    }
    return catalog;
  }

  /** Read one bounded archive-time page without scanning active alternatives. */
  async listArchivedWorkBranches(
    workId: string,
    params: WorkArchivedBranchListParamsV1 = {},
  ): Promise<WorkArchivedBranchPageV1> {
    if (
      params.limit !== undefined &&
      (!Number.isSafeInteger(params.limit) || params.limit < 1 || params.limit > 100)
    ) {
      throw new TypeError("limit must be a positive safe integer at most 100");
    }
    if (params.before !== undefined) {
      workBranchActionsPath(workId, params.before.branch_id);
      if (
        !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u.test(
          params.before.archived_at,
        ) ||
        !Number.isFinite(Date.parse(params.before.archived_at))
      ) {
        throw new TypeError("before.archived_at must be an RFC 3339 UTC timestamp");
      }
    }
    const raw = await this.fetch<unknown>(
      `${workArchivedBranchesPath(workId)}${buildQueryString({
        before_archived_at: params.before?.archived_at,
        before_branch_id: params.before?.branch_id,
        limit: params.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkArchivedBranchPageV1(raw);
    if (page.work_id !== workId) {
      throw new TypeError("Archived Work branch page identity disagrees with the request");
    }
    return page;
  }

  /** Compare two exact active branches without model ranking or inferred facts. */
  async compareWorkBranches(
    workId: string,
    leftBranchId: string,
    rightBranchId: string,
  ): Promise<WorkBranchComparisonReportV2> {
    if (leftBranchId === rightBranchId) {
      throw new TypeError("two distinct Work branches are required");
    }
    const raw = await this.post<unknown>(
      workBranchComparisonsPath(workId),
      { left_branch_id: leftBranchId, right_branch_id: rightBranchId },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const report = decodeWorkBranchComparisonReportV2(raw);
    if (
      report.work_id !== workId ||
      report.left.branch_id !== leftBranchId ||
      report.right.branch_id !== rightBranchId
    ) {
      throw new TypeError("Work branch comparison identity disagrees with the request");
    }
    return report;
  }

  /** Read immutable patch provenance without exposing the backing session. */
  async getWorkPatchArtifact(
    workId: string,
    branchId: string,
    patchArtifactId: string,
  ): Promise<WorkPatchArtifactV1> {
    const raw = await this.fetch<unknown>(
      workBranchPatchArtifactPath(workId, branchId, patchArtifactId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const artifact = decodeWorkPatchArtifactV1(raw);
    if (
      artifact.work_id !== workId ||
      artifact.branch_id !== branchId ||
      artifact.patch_artifact_id !== patchArtifactId
    ) {
      throw new TypeError("Work patch artifact identity disagrees with the request");
    }
    return artifact;
  }

  /** List bounded patch metadata; diff bodies remain lazy detail resources. */
  async listWorkPatchArtifacts(
    workId: string,
    branchId: string,
    params: WorkPatchArtifactListParamsV1 = {},
  ): Promise<WorkPatchArtifactPageV1> {
    if (
      params.limit !== undefined &&
      (!Number.isSafeInteger(params.limit) || params.limit < 1 || params.limit > 50)
    ) {
      throw new TypeError("limit must be an integer between 1 and 50");
    }
    const raw = await this.fetch<unknown>(
      `${workBranchPatchArtifactsPath(workId, branchId)}${buildQueryString({
        before_created_at: params.before?.created_at,
        before_patch_artifact_id: params.before?.patch_artifact_id,
        limit: params.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkPatchArtifactPageV1(raw);
    if (page.work_id !== workId || page.branch_id !== branchId) {
      throw new TypeError("Work patch artifact page identity disagrees with the request");
    }
    return page;
  }

  /** Read integrity-checked, bounded unified diff text for review. */
  async getWorkPatchArtifactContent(
    workId: string,
    branchId: string,
    patchArtifactId: string,
  ): Promise<WorkPatchArtifactContent> {
    return this.fetchWorkPatchContent(
      workBranchPatchArtifactContentPath(workId, branchId, patchArtifactId),
    );
  }

  /** Export the exact current Server-owned Git worktree delta as an immutable patch. */
  async exportWorkPatchArtifact(
    workId: string,
    branchId: string,
    input: WorkPatchArtifactExportInputV1,
  ): Promise<WorkPatchArtifactV1> {
    const raw = await this.post<unknown>(
      workBranchPatchArtifactsPath(workId, branchId),
      {
        request_id: input.requestId,
        expected_branch_revision: input.expectedBranchRevision,
        expected_graph_revision: input.expectedGraphRevision,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const artifact = decodeWorkPatchArtifactV1(raw);
    if (
      artifact.work_id !== workId ||
      artifact.branch_id !== branchId ||
      artifact.source_ref !== input.requestId
    ) {
      throw new TypeError("Work patch export identity disagrees with the request");
    }
    return artifact;
  }

  /** Admit an exact-base patch application; execution remains provider/policy gated. */
  async materializeWorkPatch(
    workId: string,
    targetBranchId: string,
    input: WorkPatchMaterializationInputV1,
  ): Promise<WorkPatchMaterializationOperationV2> {
    const raw = await this.post<unknown>(
      workPatchMaterializationsPath(workId, targetBranchId),
      {
        request_id: input.requestId,
        patch_artifact_id: input.patchArtifactId,
        expected_target_branch_revision: input.expectedTargetBranchRevision,
        expected_target_graph_revision: input.expectedTargetGraphRevision,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkPatchMaterializationOperationV2(raw);
    if (
      operation.work_id !== workId ||
      operation.target_branch_id !== targetBranchId ||
      operation.patch_artifact_id !== input.patchArtifactId ||
      operation.request_id !== input.requestId
    ) {
      throw new TypeError("Work patch materialization identity disagrees with the request");
    }
    return operation;
  }

  /** Restore bounded durable application progress for one branch pair. */
  async listWorkPatchMaterializations(
    workId: string,
    targetBranchId: string,
    params: WorkPatchMaterializationListParamsV1,
  ): Promise<WorkPatchMaterializationPageV2> {
    if (
      params.limit !== undefined &&
      (!Number.isSafeInteger(params.limit) || params.limit < 1 || params.limit > 50)
    ) {
      throw new TypeError("limit must be an integer between 1 and 50");
    }
    const raw = await this.fetch<unknown>(
      `${workPatchMaterializationsPath(workId, targetBranchId)}${buildQueryString({
        source_branch_id: params.sourceBranchId,
        before_created_at: params.before?.created_at,
        before_operation_id: params.before?.operation_id,
        limit: params.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkPatchMaterializationPageV2(raw);
    if (
      page.work_id !== workId ||
      page.target_branch_id !== targetBranchId ||
      page.source_branch_id !== params.sourceBranchId
    ) {
      throw new TypeError("Work patch materialization page identity disagrees with the request");
    }
    return page;
  }

  /** Read durable patch application progress without exposing executor leases. */
  async getWorkPatchMaterialization(
    workId: string,
    targetBranchId: string,
    operationId: string,
  ): Promise<WorkPatchMaterializationOperationV2> {
    const raw = await this.fetch<unknown>(
      workPatchMaterializationPath(workId, targetBranchId, operationId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkPatchMaterializationOperationV2(raw);
    if (
      operation.work_id !== workId ||
      operation.target_branch_id !== targetBranchId ||
      operation.operation_id !== operationId
    ) {
      throw new TypeError("Work patch materialization identity disagrees with the request");
    }
    return operation;
  }

  /** Abort only before dispatch; uncertain or observed effects must reconcile. */
  async abortWorkPatchMaterialization(
    workId: string,
    targetBranchId: string,
    operationId: string,
  ): Promise<void> {
    await this.fetch<void>(
      workPatchMaterializationPath(workId, targetBranchId, operationId),
      {
        method: "DELETE",
        headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
      },
    );
  }

  /** Commit one immutable reviewed patch; Git identity and provider stay Server-owned. */
  async commitWorkPatch(
    workId: string,
    targetBranchId: string,
    input: WorkPatchCommitInputV1,
  ): Promise<WorkPatchCommitOperationV1> {
    assertWorkRequestId(input.requestId);
    assertWorkCommitMessage(input.message);
    const raw = await this.post<unknown>(
      workPatchCommitsPath(workId, targetBranchId),
      {
        request_id: input.requestId,
        patch_artifact_id: input.patchArtifactId,
        expected_target_branch_revision: input.expectedTargetBranchRevision,
        expected_target_graph_revision: input.expectedTargetGraphRevision,
        message: input.message,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkPatchCommitOperationV1(raw);
    if (
      operation.work_id !== workId ||
      operation.target_branch_id !== targetBranchId ||
      operation.patch_artifact_id !== input.patchArtifactId ||
      operation.request_id !== input.requestId
    ) {
      throw new TypeError("Work patch commit identity disagrees with the request");
    }
    return operation;
  }

  /** Restore bounded durable commit progress after refresh or reconnect. */
  async listWorkPatchCommits(
    workId: string,
    targetBranchId: string,
    params: WorkPatchCommitListParamsV1 = {},
  ): Promise<WorkPatchCommitPageV1> {
    if (
      params.limit !== undefined &&
      (!Number.isSafeInteger(params.limit) || params.limit < 1 || params.limit > 50)
    ) {
      throw new TypeError("limit must be an integer between 1 and 50");
    }
    const raw = await this.fetch<unknown>(
      `${workPatchCommitsPath(workId, targetBranchId)}${buildQueryString({
        before_created_at: params.before?.created_at,
        before_operation_id: params.before?.operation_id,
        limit: params.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkPatchCommitPageV1(raw);
    if (page.work_id !== workId || page.target_branch_id !== targetBranchId) {
      throw new TypeError("Work patch commit page identity disagrees with the request");
    }
    return page;
  }

  async getWorkPatchCommit(
    workId: string,
    targetBranchId: string,
    operationId: string,
  ): Promise<WorkPatchCommitOperationV1> {
    const raw = await this.fetch<unknown>(
      workPatchCommitPath(workId, targetBranchId, operationId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkPatchCommitOperationV1(raw);
    if (
      operation.work_id !== workId ||
      operation.target_branch_id !== targetBranchId ||
      operation.operation_id !== operationId
    ) {
      throw new TypeError("Work patch commit identity disagrees with the request");
    }
    return operation;
  }

  /** Abort only before Git dispatch; possible side effects always reconcile. */
  async abortWorkPatchCommit(
    workId: string,
    targetBranchId: string,
    operationId: string,
  ): Promise<void> {
    await this.fetch<void>(workPatchCommitPath(workId, targetBranchId, operationId), {
      method: "DELETE",
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
  }

  /** Select one exact compared branch as the Work result with a pinned evidence basis. */
  async selectWorkDeliveryBranch(
    workId: string,
    input: WorkDeliverySelectionInputV1,
  ): Promise<WorkDeliverySelectionReceiptV1> {
    assertWorkRequestId(input.requestId);
    for (const [name, revision] of [
      ["expectedWorkRevision", input.expectedWorkRevision],
      ["expectedBranchRevision", input.expectedBranchRevision],
      ["expectedGoalRevision", input.expectedGoalRevision],
      ["expectedCriteriaSetRevision", input.expectedCriteriaSetRevision],
      ["expectedGraphRevision", input.expectedGraphRevision],
    ] as const) {
      if (!Number.isSafeInteger(revision) || revision < 1) {
        throw new TypeError(`${name} must be a positive safe integer`);
      }
    }
    if (
      input.expectedSubject !== null &&
      (!Number.isSafeInteger(input.expectedSubject.graphRevision) ||
        input.expectedSubject.graphRevision < 1)
    ) {
      throw new TypeError("expectedSubject.graphRevision must be a positive safe integer");
    }
    const raw = await this.post<unknown>(
      workActionsPath(workId),
      {
        request_id: input.requestId,
        expected_work_revision: input.expectedWorkRevision,
        action: {
          kind: "select_delivery_branch",
          branch_id: input.branchId,
          expected_branch_revision: input.expectedBranchRevision,
          expected_goal_revision: input.expectedGoalRevision,
          expected_criteria_set_revision: input.expectedCriteriaSetRevision,
          expected_graph_revision: input.expectedGraphRevision,
          expected_subject:
            input.expectedSubject === null
              ? null
              : {
                  graph_revision: input.expectedSubject.graphRevision,
                  subject_ref: input.expectedSubject.subjectRef,
                  subject_revision: input.expectedSubject.subjectRevision,
                },
          expected_evidence_manifest_hash: input.expectedEvidenceManifestHash,
        },
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const receipt = decodeWorkDeliverySelectionReceiptV1(raw);
    if (
      receipt.work_id !== workId ||
      receipt.request_id !== input.requestId ||
      receipt.delivery_branch_id !== input.branchId ||
      receipt.branch_revision !== input.expectedBranchRevision ||
      receipt.graph_revision !== input.expectedGraphRevision ||
      receipt.evidence_manifest_hash !== input.expectedEvidenceManifestHash
    ) {
      throw new TypeError("Work delivery selection receipt disagrees with the request");
    }
    return receipt;
  }

  /** Hide a non-delivery branch while preserving its conversation and lineage. */
  async archiveWorkBranch(
    workId: string,
    branchId: string,
    input: WorkBranchRetentionInputV1,
  ): Promise<WorkBranchRetentionReceiptV1> {
    return this.changeWorkBranchRetention(workId, branchId, "archive", input);
  }

  /** Return an archived branch to the active Work branch set. */
  async restoreWorkBranch(
    workId: string,
    branchId: string,
    input: WorkBranchRetentionInputV1,
  ): Promise<WorkBranchRetentionReceiptV1> {
    return this.changeWorkBranchRetention(workId, branchId, "restore", input);
  }

  private async changeWorkBranchRetention(
    workId: string,
    branchId: string,
    kind: "archive" | "restore",
    input: WorkBranchRetentionInputV1,
  ): Promise<WorkBranchRetentionReceiptV1> {
    assertWorkRequestId(input.requestId);
    for (const [name, revision] of [
      ["expectedWorkRevision", input.expectedWorkRevision],
      ["expectedBranchRevision", input.expectedBranchRevision],
    ] as const) {
      if (!Number.isSafeInteger(revision) || revision < 1) {
        throw new TypeError(`${name} must be a positive safe integer`);
      }
    }
    const raw = await this.post<unknown>(
      workBranchActionsPath(workId, branchId),
      {
        request_id: input.requestId,
        expected_work_revision: input.expectedWorkRevision,
        expected_branch_revision: input.expectedBranchRevision,
        action: { kind },
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const receipt = decodeWorkBranchRetentionReceiptV1(raw);
    if (
      receipt.work_id !== workId ||
      receipt.branch_id !== branchId ||
      receipt.request_id !== input.requestId ||
      receipt.kind !== kind ||
      (receipt.outcome === "applied" &&
        (receipt.work_revision !== input.expectedWorkRevision + 1 ||
          receipt.branch_revision !== input.expectedBranchRevision + 1)) ||
      (receipt.outcome === "already_in_state" &&
        (receipt.work_revision !== input.expectedWorkRevision ||
          receipt.branch_revision !== input.expectedBranchRevision))
    ) {
      throw new TypeError("Work branch retention receipt disagrees with the request");
    }
    return receipt;
  }

  /** Durably observe the current branch head without acquiring control or history. */
  async attachWorkBranch(
    workId: string,
    branchId: string,
    input: { requestId: string },
  ): Promise<WorkBranchAttachmentV1> {
    assertWorkRequestId(input.requestId);
    const raw = await this.post<unknown>(
      workBranchAttachmentsPath(workId, branchId),
      { request_id: input.requestId },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const attachment = decodeWorkBranchAttachmentV1(raw);
    if (attachment.work_id !== workId || attachment.branch_id !== branchId) {
      throw new TypeError("Work attachment identity disagrees with the requested branch");
    }
    return attachment;
  }

  /** Release read continuity without changing branch control or execution. */
  async detachWorkBranch(
    workId: string,
    branchId: string,
    attachmentId: string,
  ): Promise<void> {
    await this.fetch<void>(workBranchAttachmentPath(workId, branchId, attachmentId), {
      method: "DELETE",
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
  }

  /** Execute one explicit, revision-pinned conversation-control command. */
  async controlWorkBranch(
    workId: string,
    branchId: string,
    input: {
      requestId: string;
      expectedBranchRevision: number;
      expectedControlBasis: WorkBranchControlBasisV1;
      command: WorkBranchControlCommand;
    },
  ): Promise<WorkBranchControlOperationV2> {
    assertWorkRequestId(input.requestId);
    if (!Number.isSafeInteger(input.expectedBranchRevision) || input.expectedBranchRevision < 1) {
      throw new TypeError("expectedBranchRevision must be a positive safe integer");
    }
    if (
      !Number.isSafeInteger(input.expectedControlBasis.writer_epoch) ||
      input.expectedControlBasis.writer_epoch < 0
    ) {
      throw new TypeError("expected control writer epoch must be a non-negative safe integer");
    }
    const root = input.expectedControlBasis.canonical_root_hash;
    if (root !== null && !/^[0-9a-f]{64}$/u.test(root)) {
      throw new TypeError("expected canonical root must be a canonical SHA-256 hash");
    }
    const attachmentId = input.command.attachmentId;
    workBranchAttachmentPath(workId, branchId, attachmentId);
    const command =
      input.command.kind === "force_takeover"
        ? (() => {
            const proof = input.command.reauthenticationProof;
            if (proof.length === 0 || proof.trim() !== proof || proof.length > 160) {
              throw new TypeError("reauthenticationProof must be a bounded opaque credential");
            }
            return {
              kind: input.command.kind,
              attachment_id: attachmentId,
              reauthentication_proof: proof,
            };
          })()
        : { kind: input.command.kind, attachment_id: attachmentId };
    const raw = await this.post<unknown>(
      workBranchControlOperationsPath(workId, branchId),
      {
        request_id: input.requestId,
        expected_branch_revision: input.expectedBranchRevision,
        expected_writer_epoch: input.expectedControlBasis.writer_epoch,
        expected_canonical_root_hash: root,
        command,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchControlOperationV2(raw);
    if (operation.work_id !== workId || operation.branch_id !== branchId) {
      throw new TypeError("Work control operation identity disagrees with the requested branch");
    }
    return operation;
  }

  /** Observe a durable branch-control result without replaying its command. */
  async getWorkBranchControlOperation(
    workId: string,
    branchId: string,
    operationId: string,
  ): Promise<WorkBranchControlOperationV2> {
    const raw = await this.fetch<unknown>(
      workBranchControlOperationPath(workId, branchId, operationId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchControlOperationV2(raw);
    if (
      operation.operation_id !== operationId ||
      operation.work_id !== workId ||
      operation.branch_id !== branchId
    ) {
      throw new TypeError("Work control operation identity disagrees with the requested resource");
    }
    return operation;
  }

  /** Request cancellation; terminal operations return the server's typed conflict. */
  async abortWorkBranchControlOperation(
    workId: string,
    branchId: string,
    operationId: string,
  ): Promise<void> {
    await this.fetch<void>(workBranchControlOperationPath(workId, branchId, operationId), {
      method: "DELETE",
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
  }

  /** Create an isolated alternative from one exact committed conversation head. */
  async forkWorkBranch(
    workId: string,
    originBranchId: string,
    input: WorkBranchCreationInputV1,
  ): Promise<WorkBranchCreationOperationV1> {
    assertWorkRequestId(input.requestId);
    if (
      !Number.isSafeInteger(input.expectedBranchRevision) ||
      input.expectedBranchRevision < 1
    ) {
      throw new TypeError("expectedBranchRevision must be a positive safe integer");
    }
    const committedCursor = decodeWorkConversationHeadV1(input.committedCursor);
    const raw = await this.post<unknown>(
      workBranchForksPath(workId, originBranchId),
      {
        request_id: input.requestId,
        expected_branch_revision: input.expectedBranchRevision,
        committed_cursor: committedCursor,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchCreationOperationV1(raw);
    if (
      operation.work_id !== workId ||
      operation.origin_branch_id !== originBranchId
    ) {
      throw new TypeError("Work fork operation identity disagrees with the requested branch");
    }
    return operation;
  }

  /** Observe the exact durable fork operation without replaying branch creation. */
  async getWorkBranchForkOperation(
    workId: string,
    originBranchId: string,
    operationId: string,
  ): Promise<WorkBranchCreationOperationV1> {
    const raw = await this.fetch<unknown>(
      workBranchForkPath(workId, originBranchId, operationId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchCreationOperationV1(raw);
    if (
      operation.operation_id !== operationId ||
      operation.work_id !== workId ||
      operation.origin_branch_id !== originBranchId
    ) {
      throw new TypeError("Work fork operation identity disagrees with the requested resource");
    }
    return operation;
  }

  /** Abort only a pending fork; visible child branches are never deleted here. */
  async abortWorkBranchForkOperation(
    workId: string,
    originBranchId: string,
    operationId: string,
  ): Promise<void> {
    await this.fetch<void>(workBranchForkPath(workId, originBranchId, operationId), {
      method: "DELETE",
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
  }

  /** Irreversibly delete a non-delivery branch through its durable operation. */
  async deleteWorkBranch(
    workId: string,
    branchId: string,
    input: WorkBranchDeletionInputV1,
  ): Promise<WorkBranchDeletionOperationV1> {
    assertWorkRequestId(input.requestId);
    for (const [name, revision] of [
      ["expectedWorkRevision", input.expectedWorkRevision],
      ["expectedBranchRevision", input.expectedBranchRevision],
    ] as const) {
      if (!Number.isSafeInteger(revision) || revision < 1) {
        throw new TypeError(`${name} must be a positive safe integer`);
      }
    }
    const raw = await this.post<unknown>(
      workBranchDeletionOperationsPath(workId, branchId),
      {
        request_id: input.requestId,
        expected_work_revision: input.expectedWorkRevision,
        expected_branch_revision: input.expectedBranchRevision,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchDeletionOperationV1(raw);
    if (operation.work_id !== workId || operation.branch_id !== branchId) {
      throw new TypeError("Work deletion operation identity disagrees with the requested branch");
    }
    return operation;
  }

  /** Observe deletion convergence without replaying the destructive request. */
  async getWorkBranchDeletionOperation(
    workId: string,
    branchId: string,
    operationId: string,
  ): Promise<WorkBranchDeletionOperationV1> {
    const raw = await this.fetch<unknown>(
      workBranchDeletionOperationPath(workId, branchId, operationId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const operation = decodeWorkBranchDeletionOperationV1(raw);
    if (
      operation.operation_id !== operationId ||
      operation.work_id !== workId ||
      operation.branch_id !== branchId
    ) {
      throw new TypeError("Work deletion operation identity disagrees with the requested resource");
    }
    return operation;
  }

  /** Read one bounded chronological page from the committed branch transcript. */
  async getWorkBranchTranscript(
    workId: string,
    branchId: string,
    options: { beforeItemSeq?: number; limit?: number } = {},
  ): Promise<WorkTranscriptPageV1> {
    if (
      options.beforeItemSeq !== undefined &&
      (!Number.isSafeInteger(options.beforeItemSeq) || options.beforeItemSeq < 1)
    ) {
      throw new TypeError("beforeItemSeq must be a positive safe integer");
    }
    if (
      options.limit !== undefined &&
      (!Number.isSafeInteger(options.limit) || options.limit < 1 || options.limit > 50)
    ) {
      throw new TypeError("limit must be a safe integer between 1 and 50");
    }
    const query = new URLSearchParams();
    if (options.beforeItemSeq !== undefined) {
      query.set("before_item_seq", String(options.beforeItemSeq));
    }
    if (options.limit !== undefined) query.set("limit", String(options.limit));
    const suffix = query.size === 0 ? "" : `?${query.toString()}`;
    const raw = await this.fetch<unknown>(
      `${workBranchTranscriptPath(workId, branchId)}${suffix}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkTranscriptPageV1(raw);
    if (page.work_id !== workId || page.branch_id !== branchId) {
      throw new TypeError("Work transcript identity disagrees with the requested branch");
    }
    return page;
  }

  /** Read one bounded, revision-pinned slice of accepted Done-when criteria. */
  async getWorkCriteria(
    workId: string,
    options: { cursor?: WorkCriteriaCursorV1; limit?: number } = {},
  ): Promise<WorkCriteriaPageV1> {
    const cursor = options.cursor;
    if (
      cursor !== undefined &&
      (!Number.isSafeInteger(cursor.criteria_set_revision) ||
        cursor.criteria_set_revision < 1 ||
        !Number.isSafeInteger(cursor.offset) ||
        cursor.offset < 0 ||
        cursor.offset > 128)
    ) {
      throw new TypeError("cursor is not a bounded canonical Work criteria cursor");
    }
    if (
      options.limit !== undefined &&
      (!Number.isSafeInteger(options.limit) || options.limit < 1 || options.limit > 8)
    ) {
      throw new TypeError("limit must be a safe integer between 1 and 8");
    }
    const raw = await this.fetch<unknown>(
      `${workCriteriaPath(workId)}${buildQueryString({
        criteria_set_revision: cursor?.criteria_set_revision,
        offset: cursor?.offset,
        limit: options.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkCriteriaPageV1(raw);
    if (page.basis.work_id !== workId) {
      throw new TypeError("Work criteria page belongs to a different Work");
    }
    if (
      cursor !== undefined &&
      (page.cursor.criteria_set_revision !== cursor.criteria_set_revision ||
        page.cursor.offset !== cursor.offset)
    ) {
      throw new TypeError("Work criteria page does not continue the requested cursor");
    }
    return page;
  }

  /** Discover the bounded pending Done-when proposal inbox for one branch. */
  async listWorkCriteriaProposals(
    workId: string,
    branchId: string,
  ): Promise<WorkCriteriaProposalListV1> {
    const raw = await this.fetch<unknown>(
      workBranchCriteriaProposalsPath(workId, branchId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const list = decodeWorkCriteriaProposalListV1(raw);
    if (list.work_id !== workId || list.branch_id !== branchId) {
      throw new TypeError("Work criteria proposal inbox belongs to a different branch");
    }
    return list;
  }

  /** Load one exact provisional Done-when payload for review. */
  async getWorkCriteriaProposal(
    workId: string,
    branchId: string,
    proposalId: string,
  ): Promise<WorkCriteriaProposalDetailV1> {
    const raw = await this.fetch<unknown>(
      workBranchCriteriaProposalPath(workId, branchId, proposalId),
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const detail = decodeWorkCriteriaProposalDetailV1(raw);
    if (
      detail.proposal.work_id !== workId ||
      detail.proposal.branch_id !== branchId ||
      detail.proposal.proposal_id !== proposalId
    ) {
      throw new TypeError("Work criteria proposal response has a different identity");
    }
    return detail;
  }

  /** Accept or reject one immutable proposal using its full observed basis. */
  async resolveWorkCriteriaProposal(
    workId: string,
    branchId: string,
    proposal: WorkCriteriaProposalSummaryV1,
    input: WorkCriteriaProposalDecisionInput,
  ): Promise<WorkCriteriaProposalDetailV1> {
    const exactProposal = decodeWorkCriteriaProposalSummaryV1(proposal);
    if (
      exactProposal.work_id !== workId ||
      exactProposal.branch_id !== branchId ||
      exactProposal.status !== "pending"
    ) {
      throw new TypeError("only a pending proposal from the requested branch can be resolved");
    }
    try {
      assertWorkRequestId(input.requestId);
    } catch {
      throw new TypeError("proposal decision must have a typed action and bounded requestId");
    }
    if (input.decision !== "accept" && input.decision !== "reject") {
      throw new TypeError("proposal decision must have a typed action and bounded requestId");
    }
    const raw = await this.put<unknown>(
      workBranchCriteriaProposalDecisionPath(
        workId,
        branchId,
        exactProposal.proposal_id,
      ),
      {
        request_id: input.requestId,
        decision: input.decision,
        payload_hash: exactProposal.payload_hash,
        expected_work_revision: exactProposal.basis.work_revision,
        expected_goal_revision: exactProposal.basis.goal_revision,
        expected_criteria_set_revision: exactProposal.basis.criteria_set_revision,
        expected_branch_revision: exactProposal.basis.branch_revision,
        expected_graph_revision: exactProposal.basis.graph_revision,
      },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const detail = decodeWorkCriteriaProposalDetailV1(raw);
    if (
      detail.proposal.work_id !== workId ||
      detail.proposal.branch_id !== branchId ||
      detail.proposal.proposal_id !== exactProposal.proposal_id ||
      detail.proposal.payload_hash !== exactProposal.payload_hash ||
      detail.proposal.status !==
        (input.decision === "accept" ? "accepted" : "rejected")
    ) {
      throw new TypeError("Work criteria proposal resolution is not the requested action");
    }
    return detail;
  }

  /** Monotonically mark one exact committed Work event sequence as seen. */
  async advanceWorkReadCursor(
    workId: string,
    throughEventSeq: number,
  ): Promise<WorkReadCursorReceiptV1> {
    if (!Number.isSafeInteger(throughEventSeq) || throughEventSeq < 1) {
      throw new TypeError("throughEventSeq must be a positive safe integer");
    }
    const raw = await this.put<unknown>(
      workReadCursorPath(workId),
      { through_event_seq: throughEventSeq },
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const receipt = decodeWorkReadCursorReceiptV1(raw);
    if (receipt.work_id !== workId) {
      throw new TypeError("Work read-cursor receipt belongs to a different Work");
    }
    if (receipt.through_event_seq < throughEventSeq) {
      throw new TypeError("Work read-cursor receipt did not cover the requested event");
    }
    return receipt;
  }

  /** Read a bounded semantic event page without mutating the seen cursor. */
  async listWorkEvents(
    workId: string,
    options: { afterEventSeq?: number; limit?: number } = {},
  ): Promise<WorkEventPageV1> {
    if (
      options.afterEventSeq !== undefined &&
      (!Number.isSafeInteger(options.afterEventSeq) || options.afterEventSeq < 1)
    ) {
      throw new TypeError("afterEventSeq must be a positive safe integer");
    }
    if (
      options.limit !== undefined &&
      (!Number.isSafeInteger(options.limit) || options.limit < 1 || options.limit > 100)
    ) {
      throw new TypeError("limit must be a safe integer between 1 and 100");
    }
    const raw = await this.fetch<unknown>(
      `${workEventsPath(workId)}${buildQueryString({
        after_event_seq: options.afterEventSeq,
        limit: options.limit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkEventPageV1(raw);
    if (page.work_id !== workId) {
      throw new TypeError("Work event page belongs to a different Work");
    }
    return page;
  }

  /** Read one revision-pinned, bounded slice of declared Work items and dependencies. */
  async getWorkTaskGraph(
    workId: string,
    branchId: string,
    options: {
      cursor?: WorkTaskGraphCursorV1;
      itemLimit?: number;
      dependencyLimit?: number;
    } = {},
  ): Promise<WorkTaskGraphPageV2> {
    const cursor = options.cursor;
    if (
      cursor !== undefined &&
      (!Number.isSafeInteger(cursor.graph_revision) ||
        cursor.graph_revision < 1 ||
        !Number.isSafeInteger(cursor.item_offset) ||
        cursor.item_offset < 0 ||
        cursor.item_offset > 256 ||
        !Number.isSafeInteger(cursor.dependency_offset) ||
        cursor.dependency_offset < 0 ||
        cursor.dependency_offset > 1024)
    ) {
      throw new TypeError("cursor is not a bounded canonical Task Graph cursor");
    }
    if (
      options.itemLimit !== undefined &&
      (!Number.isSafeInteger(options.itemLimit) ||
        options.itemLimit < 1 ||
        options.itemLimit > 8)
    ) {
      throw new TypeError("itemLimit must be a safe integer between 1 and 8");
    }
    if (
      options.dependencyLimit !== undefined &&
      (!Number.isSafeInteger(options.dependencyLimit) ||
        options.dependencyLimit < 1 ||
        options.dependencyLimit > 128)
    ) {
      throw new TypeError(
        "dependencyLimit must be a safe integer between 1 and 128",
      );
    }
    const raw = await this.fetch<unknown>(
      `${workBranchTaskGraphPath(workId, branchId)}${buildQueryString({
        graph_revision: cursor?.graph_revision,
        item_offset: cursor?.item_offset,
        item_limit: options.itemLimit,
        dependency_offset: cursor?.dependency_offset,
        dependency_limit: options.dependencyLimit,
      })}`,
      { headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR } },
    );
    const page = decodeWorkTaskGraphPageV2(raw);
    if (page.basis.work_id !== workId || page.basis.branch_id !== branchId) {
      throw new TypeError("Task Graph page belongs to a different Work branch");
    }
    if (
      cursor !== undefined &&
      (page.cursor.graph_revision !== cursor.graph_revision ||
        page.cursor.item_offset !== cursor.item_offset ||
        page.cursor.dependency_offset !== cursor.dependency_offset)
    ) {
      throw new TypeError("Task Graph page does not continue the requested cursor");
    }
    return page;
  }

  /** Resolve an already-known session to the public Work branch that owns it. */
  async getWorkSessionBinding(sessionId: string): Promise<WorkSessionBindingV1> {
    const raw = await this.fetch<unknown>(workSessionBindingPath(sessionId), {
      headers: { [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR },
    });
    return decodeWorkSessionBindingV1(raw);
  }

  /** Continue one idle Work branch without exposing its backing session. */
  continueWorkBranch(
    workId: string,
    branchId: string,
    input: WorkTurnInput,
    callbacks: {
      onEvent: (event: WorkTurnStreamEvent) => void;
      onStateChange?: (state: ConnectionState) => void;
      onRawLine?: (line: string) => void;
      signal?: AbortSignal;
    },
  ): SSEClient {
    assertWorkRequestId(input.requestId);
    workBranchAttachmentPath(workId, branchId, input.attachmentId);
    const messageBytes = new TextEncoder().encode(input.message).length;
    if (
      input.message.trim().length === 0 ||
      messageBytes > 256 * 1024
    ) {
      throw new TypeError(
        "message must be non-empty and at most 262144 UTF-8 bytes",
      );
    }
    const url = this.apiPath(workBranchTurnsPath(workId, branchId));
    let client: SSEClient;
    client = new SSEClient({
      url,
      method: "POST",
      body: JSON.stringify({
        request_id: input.requestId,
        attachment_id: input.attachmentId,
        message: input.message,
      }),
      token: this.accessToken ?? undefined,
      headers: {
        ...this.config.headers,
        [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR,
      },
      onEvent: (event) => {
        try {
          callbacks.onEvent(decodeWorkTurnStreamEventV1(event));
        } catch {
          client.close();
          callbacks.onEvent({
            type: "error",
            code: "WORK_PROTOCOL_ERROR",
            message: "Work stream protocol validation failed",
            retryable: false,
          });
        }
      },
      onStateChange: callbacks.onStateChange,
      onRawLine: callbacks.onRawLine,
      decodeHttpError: async (response) => {
        const error = await readAstraError(response);
        return {
          type: "error",
          ...(error.code ? { code: error.code, error_code: error.code } : {}),
          message: error.detail,
          ...(error.retryable !== undefined
            ? { retryable: error.retryable }
            : {}),
          http_status: response.status,
          ...(error.category ? { category: error.category } : {}),
          ...(error.actionHints ? { action_hints: error.actionHints } : {}),
        };
      },
      maxRetries: 0,
      signal: callbacks.signal,
    });
    client.connect().catch(() => {});
    return client;
  }

  // ─── Sessions ──────────────────────────────────────────────────────

  async createSession(body?: {
    agentId?: string;
    title?: string;
    metadata?: Record<string, unknown>;
  }): Promise<SessionInfo> {
    const wire: Record<string, unknown> = {};
    if (body?.agentId) wire.agent_id = body.agentId;
    if (body?.title) wire.title = body.title;
    if (body?.metadata) wire.metadata = body.metadata;
    const raw = await this.post<SessionWire>(PATH_SESSIONS, wire);
    return normalizeSession(raw);
  }

  /** Raw runtime session creation for clients that need metadata/status fields. */
  async createRuntimeSession(
    body: RuntimeSessionCreateBody = {},
  ): Promise<RuntimeSessionResponse> {
    return this.post<RuntimeSessionResponse>(PATH_SESSIONS, body);
  }

  async getSession(sessionId: string): Promise<SessionInfo> {
    const raw = await this.fetch<SessionWire>(sessionPath(sessionId));
    return normalizeSession(raw);
  }

  /** Raw runtime session read for clients that need metadata/status fields. */
  async getRuntimeSession(sessionId: string): Promise<RuntimeSessionResponse> {
    return this.fetch<RuntimeSessionResponse>(sessionPath(sessionId));
  }

  async listSessions(): Promise<SessionInfo[]> {
    const raw = await this.fetch<SessionListWire>(PATH_SESSIONS);
    return raw.sessions.map(normalizeSession);
  }

  /** Raw runtime session list for clients that need pagination and metadata. */
  async listRuntimeSessions(
    params: RuntimeSessionListParams = {},
  ): Promise<RuntimeSessionListResponse> {
    const q = buildQueryString({
      ...(params.limit !== undefined ? { limit: params.limit } : {}),
      ...(params.cursor
        ? {
            after_updated_at: params.cursor.updated_at,
            after_session_id: params.cursor.session_id,
          }
        : {}),
    });
    return this.fetch<RuntimeSessionListResponse>(`${PATH_SESSIONS}${q}`);
  }

  async deleteSession(sessionId: string): Promise<void> {
    await this.fetch(sessionPath(sessionId), { method: "DELETE" });
  }

  /** Session audit summary (`GET /sessions/{id}/audit/summary`). */
  async getSessionAudit(sessionId: string): Promise<SessionAuditSummary> {
    return this.fetch<SessionAuditSummary>(sessionAuditSummaryPath(sessionId));
  }

  /** `PUT /sessions/{id}` — update title, metadata, and/or status. */
  async updateSession(
    sessionId: string,
    body: SessionUpdateBody,
  ): Promise<SessionInfo> {
    const wire: Record<string, unknown> = {};
    if (body.title !== undefined) wire.title = body.title;
    if (body.metadata !== undefined) wire.metadata = body.metadata;
    if (body.metadataPatch !== undefined) wire.metadata_patch = body.metadataPatch;
    if (body.status !== undefined) wire.status = body.status;
    const raw = await this.put<SessionWire>(sessionPath(sessionId), wire);
    return normalizeSession(raw);
  }

  /** Raw runtime session update for clients that need metadata/status fields. */
  async updateRuntimeSession(
    sessionId: string,
    body: RuntimeSessionUpdateBody,
  ): Promise<RuntimeSessionResponse> {
    return this.put<RuntimeSessionResponse>(sessionPath(sessionId), body);
  }

  async getSessionTranscript(
    sessionId: string,
    params: RuntimeTranscriptParams = {},
  ): Promise<RuntimeTranscriptResponse> {
    const q = buildQueryString({
      ...(params.before_seq !== undefined
        ? { before_seq: params.before_seq }
        : {}),
      ...(params.limit !== undefined ? { limit: params.limit } : {}),
    });
    return this.fetch<RuntimeTranscriptResponse>(
      `${sessionTranscriptPath(sessionId)}${q}`,
    );
  }

  async listSessionArtifacts(
    sessionId: string,
    params: RuntimeArtifactListParams = {},
  ): Promise<RuntimeArtifactListResponse> {
    const q = buildQueryString({
      ...(params.limit !== undefined ? { limit: params.limit } : {}),
      ...(params.offset !== undefined ? { offset: params.offset } : {}),
    });
    return this.fetch<RuntimeArtifactListResponse>(
      `${sessionArtifactsPath(sessionId)}${q}`,
    );
  }

  /** `POST /sessions/{id}/close` */
  async closeSession(sessionId: string): Promise<SessionInfo> {
    const raw = await this.post<SessionWire>(sessionClosePath(sessionId), {});
    return normalizeSession(raw);
  }

  /** `POST /sessions/{id}/resume` */
  async resumeSession(sessionId: string): Promise<SessionInfo> {
    const raw = await this.post<SessionWire>(sessionResumePath(sessionId), {});
    return normalizeSession(raw);
  }

  /** `POST /sessions/{id}/cancel` */
  async cancelSession(sessionId: string): Promise<SessionInfo> {
    const raw = await this.post<SessionWire>(sessionCancelPath(sessionId), {});
    return normalizeSession(raw);
  }

  /** `GET /sessions/{id}/activity` */
  async getSessionActivity(
    sessionId: string,
    opts?: { limit?: number; cursor?: SessionActivityCursor },
  ): Promise<SessionActivityResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts?.cursor
        ? {
            after_created_at: opts.cursor.created_at,
            after_log_id: opts.cursor.log_id,
          }
        : {}),
    });
    return this.fetch<SessionActivityResponse>(
      `${sessionActivityPath(sessionId)}${q}`,
    );
  }

  /** `GET /chat/session/{id}/reflect` */
  async getSessionReflect(
    sessionId: string,
    params?: ReflectQueryParams,
  ): Promise<ReflectReport> {
    const q = buildQueryString({
      ...(params?.focus !== undefined ? { focus: params.focus } : {}),
      ...(params?.last_n !== undefined ? { last_n: params.last_n } : {}),
      ...(params?.question !== undefined ? { question: params.question } : {}),
    });
    const raw = await this.fetch<unknown>(
      `${chatSessionReflectPath(sessionId)}${q}`,
    );
    return normalizeReflectReport(raw, {
      sessionId,
      focus: params?.focus ?? "auto",
    });
  }

  /** `GET /chat/session/{id}/decision-trace` (server uses focus `tool_surface`). */
  async getSessionDecisionTrace(
    sessionId: string,
    params?: ReflectQueryParams,
  ): Promise<ReflectReport> {
    const q = buildQueryString({
      ...(params?.focus !== undefined ? { focus: params.focus } : {}),
      ...(params?.last_n !== undefined ? { last_n: params.last_n } : {}),
      ...(params?.question !== undefined ? { question: params.question } : {}),
    });
    const raw = await this.fetch<unknown>(
      `${chatSessionDecisionTracePath(sessionId)}${q}`,
    );
    return normalizeReflectReport(raw, {
      sessionId,
      focus: params?.focus ?? "tool_surface",
    });
  }

  /** `GET /events/session/{session_id}` */
  async getSessionEvents(
    sessionId: string,
    opts?: { limit?: number; cursor?: EventListCursor },
  ): Promise<EventListResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts?.cursor
        ? {
            after_created_at: opts.cursor.created_at,
            after_event_id: opts.cursor.event_id,
          }
        : {}),
    });
    const raw = await this.fetch<EventListResponse>(
      `${eventsSessionPath(sessionId)}${q}`,
    );
    return normalizeEventList(raw);
  }

  /** `GET /events` — cross-session filter. */
  async listEvents(filters?: EventListFilters): Promise<EventListResponse> {
    const q = buildQueryString({
      ...(filters?.sessionId ? { session_id: filters.sessionId } : {}),
      ...(filters?.eventType ? { event_type: filters.eventType } : {}),
      ...(filters?.agentId ? { agent_id: filters.agentId } : {}),
      ...(filters?.causalChainId
        ? { causal_chain_id: filters.causalChainId }
        : {}),
      ...(filters?.limit !== undefined ? { limit: filters.limit } : {}),
      ...(filters?.cursor
        ? {
            after_created_at: filters.cursor.created_at,
            after_event_id: filters.cursor.event_id,
          }
        : {}),
    });
    const raw = await this.fetch<EventListResponse>(`${PATH_EVENTS}${q}`);
    return normalizeEventList(raw);
  }

  /** `GET /events/causal-chain/{id}` */
  async getCausalChain(causalChainId: string): Promise<EventResponse[]> {
    const raw = await this.fetch<EventResponse[]>(
      eventsCausalChainPath(causalChainId),
    );
    return raw.map((ev) => ({
      ...ev,
      metadata:
        ev.metadata &&
        typeof ev.metadata === "object" &&
        !Array.isArray(ev.metadata)
          ? (ev.metadata as Record<string, unknown>)
          : {},
    }));
  }

  /** `GET /edges/status` — connected edge executors for the current user. */
  async getEdgesStatus(): Promise<EdgeStatusResponse> {
    return this.fetch<EdgeStatusResponse>(PATH_EDGES_STATUS);
  }

  // ─── Runs ──────────────────────────────────────────────────────────

  /** Non-streaming chat turn — `POST /chat`. */
  async createRun(
    request: ChatRequest,
    init?: RequestInit,
  ): Promise<RunStatus> {
    const raw = await this.post<ChatResponseWire>(
      PATH_CHAT,
      chatRequestToWire(request),
      init,
    );
    return {
      runId: raw.run_id,
      sessionId: raw.session_id,
      parentRunId: null,
      rootRunId: raw.run_id,
      depth: 0,
      status: raw.status,
      eventsCount: 0,
    };
  }

  async getRunStatus(runId: string): Promise<RunStatus> {
    const raw = await this.fetch<RunStatusWire>(chatRunPath(runId));
    return normalizeRunStatus(raw);
  }

  async cancelRun(runId: string): Promise<void> {
    await this.fetch(chatRunPath(runId), { method: "DELETE" });
  }

  async pauseRun(runId: string): Promise<void> {
    await this.post(chatRunPausePath(runId));
  }

  async submitRunInput(
    runId: string,
    body: RunInputRequestBody,
  ): Promise<RunInputResponse> {
    const raw = await this.post<RunInputResponseWire>(chatRunInputPath(runId), {
      idempotency_key: body.idempotencyKey,
      input: body.input,
    });
    return normalizeRunInputResponse(raw);
  }

  async resumeRun(runId: string): Promise<void> {
    await this.post(chatRunResumePath(runId));
  }

  /**
   * Fetch run events from `GET /chat/runs/{id}/stream` (buffered SSE).
   * `startIndex` is sent as `last_index` to the server.
   */
  async getRunEvents(runId: string, startIndex = 0): Promise<StreamEvent[]> {
    const path = `${chatRunStreamPath(runId)}?last_index=${encodeURIComponent(String(startIndex))}`;
    const url = this.apiPath(path);
    const streamHeaders = this.buildHeaders(undefined, {
      Accept: "text/event-stream",
    });
    let res = await fetch(url, { headers: streamHeaders });

    if (res.status === 401) {
      const refreshed = await this.tryRefreshToken();
      if (refreshed) {
        res = await fetch(url, {
          headers: this.buildHeaders(undefined, {
            Accept: "text/event-stream",
          }),
        });
      }
    }

    if (!res.ok) {
      const body = await readAstraErrorDetail(res);
      throw new AstraApiError(res.status, body, path);
    }

    const text = await res.text();
    return parseSseDataEvents(text);
  }

  /**
   * Fetch the bounded durable projection for a run without attaching to its
   * live SSE stream.
   */
  async getRunProjection(
    runId: string,
    opts?: { recentLimit?: number },
  ): Promise<RunProjectionResponse> {
    const q = buildQueryString({
      ...(opts?.recentLimit !== undefined
        ? { recent_limit: opts.recentLimit }
        : {}),
    });
    return this.fetch<RunProjectionResponse>(
      `${chatRunProjectionPath(runId)}${q}`,
    );
  }

  /**
   * Rebuild the durable projection for a run from authoritative facts.
   */
  async repairRunProjection(
    runId: string,
    opts?: { recentLimit?: number },
  ): Promise<RunProjectionRepairResponse> {
    const q = buildQueryString({
      ...(opts?.recentLimit !== undefined
        ? { recent_limit: opts.recentLimit }
        : {}),
    });
    return this.post<RunProjectionRepairResponse>(
      `${chatRunProjectionRepairPath(runId)}${q}`,
    );
  }

  /** `GET /runs` — list durable runs for the current user. */
  async listRuns(opts: RunListParams = {}): Promise<RunListResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts.cursor
        ? {
            after_updated_at: opts.cursor.updatedAt,
            after_run_id: opts.cursor.runId,
          }
        : {}),
    });
    const raw = await this.fetch<RunListWire>(`${PATH_RUNS}${q}`);
    return normalizeRunList(raw);
  }

  /** `POST /chat/runs/{run_id}/delegate` — multi-agent coordination. */
  async delegateRun(
    runId: string,
    body: DelegationRequestBody,
  ): Promise<DelegationResponse> {
    return this.post<DelegationResponse>(chatRunDelegatePath(runId), body);
  }

  /** `GET /chat/runs/{run_id}/delegations` — sub-run ids for this parent run. */
  async listDelegations(runId: string): Promise<DelegationListResponse> {
    return this.fetch<DelegationListResponse>(chatRunDelegationsPath(runId));
  }

  /** `POST /chat/runs/{run_id}/delegations/pause` */
  async pauseDelegations(runId: string): Promise<DelegationMutationResponse> {
    return this.post<DelegationMutationResponse>(
      chatRunDelegationsPausePath(runId),
    );
  }

  /** `POST /chat/runs/{run_id}/delegations/resume` */
  async resumeDelegations(runId: string): Promise<DelegationMutationResponse> {
    return this.post<DelegationMutationResponse>(
      chatRunDelegationsResumePath(runId),
    );
  }

  // ─── Memory ─────────────────────────────────────────────────────────

  async memoryStore(entry: MemoryEntry): Promise<{ id: string }> {
    return this.post<{ id: string }>(PATH_MEMORY_STORE, entry);
  }

  async memorySearch(query: string, topK = 10): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>(PATH_MEMORY_SEARCH, {
      query,
      top_k: topK,
    });
  }

  async memoryRetrieve(query: string, topK = 5): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>(PATH_MEMORY_RETRIEVE, {
      query,
      top_k: topK,
    });
  }

  async memoryPurge(topic: string): Promise<void> {
    await this.post(PATH_MEMORY_PURGE, { topic });
  }

  // ─── Models ─────────────────────────────────────────────────────────

  async listModels(): Promise<RuntimeModelListItem[]> {
    const items: RuntimeModelListItem[] = [];
    let cursor: RuntimeModelCatalogCursor | null = null;
    let total: number | null = null;
    let revision: string | null = null;
    const seenCursors = new Set<string>();
    do {
      if (cursor) {
        const key = JSON.stringify(cursor);
        if (seenCursors.has(key)) {
          throw new Error("Model catalog cycled its continuation cursor");
        }
        seenCursors.add(key);
      }
      const query = new URLSearchParams({ limit: "200" });
      if (cursor) {
        query.set("after_provider", cursor.provider);
        query.set("after_name", cursor.model_name);
        query.set("after_offering_id", cursor.model_id);
      }
      const page = await this.fetch<RuntimeModelListPageResponse>(
        `${PATH_MODELS}?${query.toString()}`,
      );
      if (page.limit <= 0 || page.limit > 200) {
        throw new Error("Invalid model catalog page limit");
      }
      if (total !== null && total !== page.total) {
        throw new Error("Model catalog total changed during pagination");
      }
      if (revision !== null && revision !== page.catalog_revision) {
        throw new Error("Model catalog revision changed during pagination");
      }
      total ??= page.total;
      revision ??= page.catalog_revision;
      items.push(...page.items);
      const next = page.next_cursor;
      if (next && page.items.length === 0) {
        throw new Error("Model catalog returned a cursor without items");
      }
      if (next && cursor && JSON.stringify(next) === JSON.stringify(cursor)) {
        throw new Error("Model catalog repeated its continuation cursor");
      }
      cursor = next;
    } while (cursor);
    if (total !== null && items.length !== total) {
      throw new Error(
        `Model catalog returned ${items.length} items but advertised ${total}`,
      );
    }
    return items;
  }

  async getModelAccess(): Promise<RuntimeModelAccessProjection> {
    let cursor: RuntimeModelCatalogCursor | null = null;
    let result: RuntimeModelAccessProjection | null = null;
    const seenCursors = new Set<string>();
    do {
      if (cursor) {
        const key = JSON.stringify(cursor);
        if (seenCursors.has(key)) {
          throw new Error("Model Access cycled its continuation cursor");
        }
        seenCursors.add(key);
      }
      const query = new URLSearchParams({ limit: "200" });
      if (cursor) {
        query.set("after_provider", cursor.provider);
        query.set("after_name", cursor.model_name);
        query.set("after_offering_id", cursor.model_id);
      }
      const page = await this.fetch<RuntimeModelAccessProjection>(
        `${PATH_MODEL_ACCESS}?${query.toString()}`,
      );
      if (page.limit <= 0 || page.limit > 200) {
        throw new Error("Invalid Model Access page limit");
      }
      if (result && result.catalog_revision !== page.catalog_revision) {
        throw new Error(
          "Model Access catalog revision changed during pagination",
        );
      }
      if (result && result.total !== page.total) {
        throw new Error("Model Access total changed during pagination");
      }
      if (!result) {
        result = page;
      } else {
        if (
          JSON.stringify(result.accesses) !== JSON.stringify(page.accesses)
        ) {
          throw new Error(
            "Model Access declarations changed during pagination",
          );
        }
        result.offerings.push(...page.offerings);
        result.next_cursor = page.next_cursor;
      }
      const next = page.next_cursor;
      if (next && page.offerings.length === 0) {
        throw new Error("Model Access returned a cursor without offerings");
      }
      if (next && cursor && JSON.stringify(next) === JSON.stringify(cursor)) {
        throw new Error("Model Access repeated its continuation cursor");
      }
      cursor = next;
    } while (cursor);
    if (!result) {
      throw new Error("Model Access returned no page");
    }
    if (result.offerings.length !== result.total) {
      throw new Error(
        `Model Access returned ${result.offerings.length} offerings but advertised ${result.total}`,
      );
    }
    for (const access of result.accesses) {
      const count = result.offerings.filter(
        (offering) => offering.access_id === access.id,
      ).length;
      if (count !== access.available_model_count) {
        throw new Error(
          `Model Access count for ${access.id} advertised ${access.available_model_count} but drained ${count}`,
        );
      }
    }
    result.next_cursor = null;
    return result;
  }

  // ─── Agent Binding Registry ───────────────────────────────────────

  async createAgentBinding(
    body: AgentBindingCreateRequest,
  ): Promise<AgentBindingCreateResponse> {
    return this.post<AgentBindingCreateResponse>(PATH_AGENT_BINDINGS, body);
  }

  async getAgentBinding(agentBindingId: string): Promise<AgentBindingRecord> {
    return this.fetch<AgentBindingRecord>(agentBindingPath(agentBindingId));
  }

  async disableAgentBinding(
    agentBindingId: string,
  ): Promise<AgentBindingRecord> {
    return this.post<AgentBindingRecord>(
      agentBindingDisablePath(agentBindingId),
      {},
    );
  }

  // ─── Skills ─────────────────────────────────────────────────────────

  /** Raw runtime skill catalog list for clients that need pagination/source/category metadata. */
  async listRuntimeSkills(
    params: RuntimeSkillListParams = {},
  ): Promise<RuntimeSkillListResponse> {
    const q = buildQueryString({
      ...(params.limit !== undefined ? { limit: params.limit } : {}),
      ...(params.cursor
        ? {
            after_skill_name: params.cursor.skill_name,
            after_version: params.cursor.version,
            after_skill_id: params.cursor.skill_id,
          }
        : {}),
    });
    return this.fetch<RuntimeSkillListResponse>(`${PATH_SKILLS}${q}`);
  }

  async listSkills(): Promise<SkillInfo[]> {
    const raw = await this.listRuntimeSkills();
    return (raw.skills ?? [])
      .map((s) => ({
        id: s.skill_id ?? s.skill_name ?? "",
        name: s.skill_name ?? s.skill_id ?? "",
        description: s.description ?? "",
        status: s.status ?? s.version ?? "",
      }))
      .filter((s) => s.name.length > 0);
  }

  /** `POST /skills` — register a skill draft (returns `SkillRecord`, HTTP 201). */
  async registerSkill(body: RegisterSkillBody): Promise<SkillRecord> {
    return this.post<SkillRecord>(PATH_SKILLS, body);
  }

  /** `POST /skills/publish` — publish a skill version (HTTP 201, JSON payload varies). */
  async publishSkill(body: PublishSkillBody): Promise<unknown> {
    return this.post(PATH_SKILLS_PUBLISH, body);
  }

  /** `GET /skills/{skill_id}` — optional `version` query. */
  async getSkill(
    skillId: string,
    opts?: { version?: string },
  ): Promise<SkillRecord> {
    const q = buildQueryString(
      opts?.version !== undefined ? { version: opts.version } : {},
    );
    return this.fetch<SkillRecord>(`${skillPath(skillId)}${q}`);
  }

  /** `POST /skills/{skill_name}/unpublish` */
  async unpublishSkill(skillName: string): Promise<unknown> {
    return this.post(skillUnpublishPath(skillName), {});
  }

  // ─── Streaming ─────────────────────────────────────────────────────

  /**
   * Stream a chat message via `POST /chat/stream` (JSON body, SSE response).
   *
   * Returns an SSEClient that can be closed to abort the stream.
   */
  streamChat(
    request: ChatRequest,
    callbacks: {
      onEvent: (event: StreamEvent) => void;
      onStateChange?: (state: ConnectionState) => void;
      onRawLine?: (line: string) => void;
      signal?: AbortSignal;
    },
  ): SSEClient {
    const url = this.apiPath(PATH_CHAT_STREAM);
    const client = new SSEClient({
      url,
      method: "POST",
      body: JSON.stringify(chatRequestToWire(request)),
      token: this.accessToken ?? undefined,
      headers: this.config.headers,
      onEvent: callbacks.onEvent,
      onStateChange: callbacks.onStateChange,
      onRawLine: callbacks.onRawLine,
      maxRetries: 0,
      requireTerminalEvent: true,
      signal: callbacks.signal,
    });

    client.connect().catch(() => {});
    return client;
  }

  // ─── §5.5 Edge callbacks ───────────────────────────────────────────

  async postToolResult(
    body: ToolResultRequestBody,
    options?: { edgeExecutorId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeExecutorId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeExecutorId;
    }
    return this.fetch(PATH_TOOLS_RESULT, {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }

  async postApprovalRespond(
    body: ApprovalRespondRequestBody,
  ): Promise<unknown> {
    return this.post(PATH_APPROVAL_RESPOND, body);
  }

  async registerEdge(
    body: EdgeRegisterRequestBody,
    options?: { edgeTransportId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeTransportId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeTransportId;
    }
    return this.fetch(PATH_AGENTS_EDGE, {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }

  async postEdgeHeartbeat(
    body: EdgeHeartbeatRequestBody,
    options?: { edgeTransportId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeTransportId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeTransportId;
    }
    return this.fetch(PATH_AGENTS_EDGE_HEARTBEAT, {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }

  /**
   * @deprecated Legacy compatibility path. The current Astra runtime does
   * not register `/tasks/{id}/lease`; this only works against older servers.
   */
  async getTaskLease(taskId: string): Promise<unknown> {
    return this.fetch(taskLeasePath(taskId));
  }

  /** @deprecated Legacy compatibility path; not registered by the current runtime. */
  async postTaskLeaseClaim(
    taskId: string,
    body: TaskLeaseMutationRequestBody,
    options?: { edgeTransportId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeTransportId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeTransportId;
    }
    return this.fetch(taskLeaseClaimPath(taskId), {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }

  /** @deprecated Legacy compatibility path; not registered by the current runtime. */
  async postTaskLeaseRelease(
    taskId: string,
    body: TaskLeaseMutationRequestBody,
    options?: { edgeTransportId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeTransportId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeTransportId;
    }
    return this.fetch(taskLeaseReleasePath(taskId), {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }

  /** @deprecated Legacy compatibility path; not registered by the current runtime. */
  async postTaskLeaseRenew(
    taskId: string,
    body: TaskLeaseMutationRequestBody,
    options?: { edgeTransportId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeTransportId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeTransportId;
    }
    return this.fetch(taskLeaseRenewPath(taskId), {
      method: "POST",
      body: JSON.stringify(body),
      headers,
    });
  }
}

// ─── Errors ────────────────────────────────────────────────────────

export class AstraApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly body: string,
    public readonly path: string,
    public readonly code?: string,
    public readonly category?: string,
    public readonly retryable?: boolean,
    public readonly actionHints?: string[],
  ) {
    super(`Astra API error ${status} on ${path}: ${body.slice(0, 200)}`);
    this.name = "AstraApiError";
  }
}
