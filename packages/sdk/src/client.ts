import type {
  AstraClientConfig,
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
  EventListFilters,
  EventListResponse,
  EventResponse,
  MemoryEntry,
  MemorySearchResult,
  ReflectQueryParams,
  ReflectReport,
  RegisterSkillBody,
  PublishSkillBody,
  RunListResponse,
  RunStatus,
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
} from './types';
import {
  ASTRA_EDGE_ID_HEADER,
  PATH_AGENTS_EDGE,
  PATH_AGENTS_EDGE_HEARTBEAT,
  PATH_APPROVAL_RESPOND,
  PATH_AUTH_LOGIN,
  PATH_AUTH_LOGOUT,
  PATH_AUTH_ME,
  PATH_AUTH_REFRESH,
  PATH_AUTH_REGISTER,
  PATH_CHAT,
  PATH_CHAT_STREAM,
  PATH_EDGES_STATUS,
  PATH_EVENTS,
  PATH_MEMORY_PURGE,
  PATH_MEMORY_RETRIEVE,
  PATH_MEMORY_SEARCH,
  PATH_MEMORY_STORE,
  PATH_RUNS,
  PATH_SESSIONS,
  PATH_SKILLS,
  PATH_SKILLS_PUBLISH,
  PATH_TOOLS_RESULT,
  buildQueryString,
  chatRunDelegatePath,
  chatRunDelegationsPath,
  chatRunDelegationsPausePath,
  chatRunDelegationsResumePath,
  chatRunPath,
  chatRunPausePath,
  chatRunResumePath,
  chatRunStreamPath,
  chatSessionDecisionTracePath,
  chatSessionReflectPath,
  eventsCausalChainPath,
  eventsSessionPath,
  joinApiPath,
  sessionActivityPath,
  sessionAuditSummaryPath,
  sessionCancelPath,
  sessionClosePath,
  sessionPath,
  sessionResumePath,
  skillPath,
  skillUnpublishPath,
  taskLeaseClaimPath,
  taskLeasePath,
  taskLeaseReleasePath,
  taskLeaseRenewPath,
} from './paths';
import { SSEClient, parseSseDataEvents } from './sse-client';

/** `Headers` or undici/VM instances where `instanceof Headers` is unreliable. */
function isWebHeadersObject(h: unknown): h is Headers {
  if (h == null || typeof h !== 'object' || Array.isArray(h)) return false;
  if (h instanceof Headers) return true;
  return (
    'append' in h &&
    'forEach' in h &&
    typeof (h as Headers).forEach === 'function' &&
    typeof (h as Headers).append === 'function'
  );
}

/** Merge `RequestInit.headers` into a plain record (handles `Headers` and `[string, string][]`). */
function mergeHeadersInit(
  base: Record<string, string>,
  initHeaders?: HeadersInit,
): Record<string, string> {
  if (initHeaders == null) return { ...base };
  if (Array.isArray(initHeaders)) {
    const out = { ...base };
    for (const [k, v] of initHeaders) {
      out[k] = v;
    }
    return out;
  }
  if (isWebHeadersObject(initHeaders)) {
    const out = { ...base };
    initHeaders.forEach((value, key) => {
      out[key] = value;
    });
    return out;
  }
  return { ...base, ...(initHeaders as Record<string, string>) };
}

type SessionWire = {
  session_id: string;
  user_id?: string;
  agent_id?: string | null;
  title?: string | null;
  status?: string;
  event_count?: number;
  created_at: string;
  updated_at?: string | null;
  ended_at?: string | null;
  metadata?: Record<string, unknown>;
};

type SessionListWire = {
  sessions: SessionWire[];
  total: number;
  limit: number;
  offset: number;
};

type RunStatusWire = {
  run_id: string;
  session_id: string;
  status: string;
  waiting_for?: string | null;
  events_count: number;
};

type RunListWire = {
  runs: RunStatusWire[];
  total: number;
  limit: number;
  offset: number;
};

type ChatResponseWire = {
  session_id: string;
  run_id: string;
  status: string;
};

type SkillListItemWire = {
  skill_id: string;
  skill_name: string;
  version: string;
  description?: string | null;
  status?: string | null;
};

type SkillListWire = {
  skills: SkillListItemWire[];
  total: number;
  limit: number;
  offset: number;
};

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
    status: w.status as RunStatus['status'],
    eventsCount: Number(w.events_count),
    waitingFor: w.waiting_for ?? undefined,
  };
}

function normalizeRunList(w: RunListWire): RunListResponse {
  return {
    runs: w.runs.map(normalizeRunStatus),
    total: w.total,
    limit: w.limit,
    offset: w.offset,
  };
}

function normalizeEventList(raw: EventListResponse): EventListResponse {
  return {
    ...raw,
    events: raw.events.map((ev) => ({
      ...ev,
      metadata:
        ev.metadata && typeof ev.metadata === 'object' && !Array.isArray(ev.metadata)
          ? (ev.metadata as Record<string, unknown>)
          : {},
    })),
  };
}

export function chatRequestToWire(req: ChatRequest): Record<string, unknown> {
  const body: Record<string, unknown> = {
    message: req.message,
    max_candidates: req.maxCandidates ?? 8,
  };
  if (req.sessionId) body.session_id = req.sessionId;
  if (req.agentId) body.agent_id = req.agentId;
  if (req.model) body.model = req.model;
  if (req.context) body.context = req.context;
  if (req.explain !== undefined) body.explain = req.explain;
  if (req.planSubtaskId) body.plan_subtask_id = req.planSubtaskId;
  if (req.isPlanSubtask !== undefined) body.is_plan_subtask = req.isPlanSubtask;
  if (req.edgeExecutorId) body.edge_executor_id = req.edgeExecutorId;
  if (req.capabilities?.length) body.capabilities = req.capabilities;
  if (req.allowSkills?.length) body.allow_skills = req.allowSkills;
  if (req.allowTools?.length) body.allow_tools = req.allowTools;
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
    const base = this.config.baseUrl.replace(/\/$/, '');
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
    const result = await this.post<AuthResult>(PATH_AUTH_LOGIN, { username, password });
    this.accessToken = result.access_token;
    this.refreshTokenValue = result.refresh_token;
    return result;
  }

  /** Log out and clear stored tokens. Requires refresh token in client state. */
  async logout(): Promise<void> {
    try {
      if (this.refreshTokenValue) {
        await this.post(PATH_AUTH_LOGOUT, { refresh_token: this.refreshTokenValue });
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

  setTokens(accessToken: string, refreshToken?: string): void {
    this.accessToken = accessToken;
    if (refreshToken) this.refreshTokenValue = refreshToken;
  }

  private async tryRefreshToken(): Promise<boolean> {
    if (!this.refreshTokenValue) return false;
    try {
      const res = await fetch(this.apiPath(PATH_AUTH_REFRESH), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: this.refreshTokenValue }),
      });
      if (!res.ok) return false;
      const data = (await res.json()) as AuthResult;
      this.accessToken = data.access_token;
      this.refreshTokenValue = data.refresh_token;
      this.config.onTokenRefresh?.({
        accessToken: data.access_token,
        refreshToken: data.refresh_token,
      });
      return true;
    } catch {
      return false;
    }
  }

  private buildHeaders(init?: RequestInit, extra?: Record<string, string>): Record<string, string> {
    const headers: Record<string, string> = {
      ...this.config.headers,
      ...extra,
    };
    const method = (init?.method ?? 'GET').toUpperCase();
    if (
      init?.body != null &&
      (method === 'POST' || method === 'PUT' || method === 'PATCH' || method === 'DELETE')
    ) {
      headers['Content-Type'] = 'application/json';
    }
    if (this.accessToken) {
      headers['Authorization'] = `Bearer ${this.accessToken}`;
    }
    return headers;
  }

  // ─── HTTP helpers ──────────────────────────────────────────────────

  async fetch<T>(path: string, init?: RequestInit): Promise<T> {
    const url = this.apiPath(path);
    let res = await fetch(url, {
      ...init,
      headers: mergeHeadersInit(this.buildHeaders(init), init?.headers),
    });

    if (res.status === 401) {
      const refreshed = await this.tryRefreshToken();
      if (refreshed) {
        res = await fetch(url, {
          ...init,
          headers: mergeHeadersInit(this.buildHeaders(init), init?.headers),
        });
      }
    }

    if (!res.ok) {
      const body = await res.text().catch(() => '');
      throw new AstraApiError(res.status, body, path);
    }

    if (res.status === 204 || res.headers.get('content-length') === '0') {
      return undefined as T;
    }

    const text = await res.text();
    if (!text) return undefined as T;
    try {
      return JSON.parse(text) as T;
    } catch (parseErr) {
      const msg = parseErr instanceof Error ? parseErr.message : String(parseErr);
      throw new AstraApiError(
        res.status,
        `Invalid JSON response: ${msg}; body starts: ${text.slice(0, 500)}`,
        path,
      );
    }
  }

  async post<T>(path: string, body?: unknown): Promise<T> {
    return this.fetch<T>(path, {
      method: 'POST',
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  }

  async put<T>(path: string, body?: unknown): Promise<T> {
    return this.fetch<T>(path, {
      method: 'PUT',
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
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

  async getSession(sessionId: string): Promise<SessionInfo> {
    const raw = await this.fetch<SessionWire>(sessionPath(sessionId));
    return normalizeSession(raw);
  }

  async listSessions(): Promise<SessionInfo[]> {
    const raw = await this.fetch<SessionListWire>(PATH_SESSIONS);
    return raw.sessions.map(normalizeSession);
  }

  async deleteSession(sessionId: string): Promise<void> {
    await this.fetch(sessionPath(sessionId), { method: 'DELETE' });
  }

  /** Session audit summary (`GET /sessions/{id}/audit/summary`). */
  async getSessionAudit(sessionId: string): Promise<SessionAuditSummary> {
    return this.fetch<SessionAuditSummary>(sessionAuditSummaryPath(sessionId));
  }

  /** `PUT /sessions/{id}` — update title, metadata, and/or status. */
  async updateSession(sessionId: string, body: SessionUpdateBody): Promise<SessionInfo> {
    const wire: Record<string, unknown> = {};
    if (body.title !== undefined) wire.title = body.title;
    if (body.metadata !== undefined) wire.metadata = body.metadata;
    if (body.status !== undefined) wire.status = body.status;
    const raw = await this.put<SessionWire>(sessionPath(sessionId), wire);
    return normalizeSession(raw);
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
    opts?: { limit?: number; offset?: number },
  ): Promise<SessionActivityResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts?.offset !== undefined ? { offset: opts.offset } : {}),
    });
    return this.fetch<SessionActivityResponse>(`${sessionActivityPath(sessionId)}${q}`);
  }

  /** `GET /chat/session/{id}/reflect` */
  async getSessionReflect(sessionId: string, params?: ReflectQueryParams): Promise<ReflectReport> {
    const q = buildQueryString({
      ...(params?.focus !== undefined ? { focus: params.focus } : {}),
      ...(params?.last_n !== undefined ? { last_n: params.last_n } : {}),
      ...(params?.question !== undefined ? { question: params.question } : {}),
    });
    return this.fetch<ReflectReport>(`${chatSessionReflectPath(sessionId)}${q}`);
  }

  /** `GET /chat/session/{id}/decision-trace` (server uses focus `tool_selection`). */
  async getSessionDecisionTrace(
    sessionId: string,
    params?: ReflectQueryParams,
  ): Promise<ReflectReport> {
    const q = buildQueryString({
      ...(params?.focus !== undefined ? { focus: params.focus } : {}),
      ...(params?.last_n !== undefined ? { last_n: params.last_n } : {}),
      ...(params?.question !== undefined ? { question: params.question } : {}),
    });
    return this.fetch<ReflectReport>(`${chatSessionDecisionTracePath(sessionId)}${q}`);
  }

  /** `GET /events/session/{session_id}` */
  async getSessionEvents(
    sessionId: string,
    opts?: { limit?: number; offset?: number },
  ): Promise<EventListResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts?.offset !== undefined ? { offset: opts.offset } : {}),
    });
    const raw = await this.fetch<EventListResponse>(`${eventsSessionPath(sessionId)}${q}`);
    return normalizeEventList(raw);
  }

  /** `GET /events` — cross-session filter. */
  async listEvents(filters?: EventListFilters): Promise<EventListResponse> {
    const q = buildQueryString({
      ...(filters?.sessionId ? { session_id: filters.sessionId } : {}),
      ...(filters?.eventType ? { event_type: filters.eventType } : {}),
      ...(filters?.agentId ? { agent_id: filters.agentId } : {}),
      ...(filters?.causalChainId ? { causal_chain_id: filters.causalChainId } : {}),
      ...(filters?.limit !== undefined ? { limit: filters.limit } : {}),
      ...(filters?.offset !== undefined ? { offset: filters.offset } : {}),
    });
    const raw = await this.fetch<EventListResponse>(`${PATH_EVENTS}${q}`);
    return normalizeEventList(raw);
  }

  /** `GET /events/causal-chain/{id}` */
  async getCausalChain(causalChainId: string): Promise<EventResponse[]> {
    const raw = await this.fetch<EventResponse[]>(eventsCausalChainPath(causalChainId));
    return raw.map((ev) => ({
      ...ev,
      metadata:
        ev.metadata && typeof ev.metadata === 'object' && !Array.isArray(ev.metadata)
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
  async createRun(request: ChatRequest): Promise<RunStatus> {
    const raw = await this.post<ChatResponseWire>(PATH_CHAT, chatRequestToWire(request));
    return {
      runId: raw.run_id,
      sessionId: raw.session_id,
      status: raw.status,
      eventsCount: 0,
    };
  }

  async getRunStatus(runId: string): Promise<RunStatus> {
    const raw = await this.fetch<RunStatusWire>(chatRunPath(runId));
    return normalizeRunStatus(raw);
  }

  async cancelRun(runId: string): Promise<void> {
    await this.fetch(chatRunPath(runId), { method: 'DELETE' });
  }

  async pauseRun(runId: string): Promise<void> {
    await this.post(chatRunPausePath(runId));
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
    const streamHeaders = this.buildHeaders(undefined, { Accept: 'text/event-stream' });
    let res = await fetch(url, { headers: streamHeaders });

    if (res.status === 401) {
      const refreshed = await this.tryRefreshToken();
      if (refreshed) {
        res = await fetch(url, {
          headers: this.buildHeaders(undefined, { Accept: 'text/event-stream' }),
        });
      }
    }

    if (!res.ok) {
      const body = await res.text().catch(() => '');
      throw new AstraApiError(res.status, body, path);
    }

    const text = await res.text();
    return parseSseDataEvents(text);
  }

  /** `GET /runs` — list durable runs for the current user. */
  async listRuns(opts?: { limit?: number; offset?: number }): Promise<RunListResponse> {
    const q = buildQueryString({
      ...(opts?.limit !== undefined ? { limit: opts.limit } : {}),
      ...(opts?.offset !== undefined ? { offset: opts.offset } : {}),
    });
    const raw = await this.fetch<RunListWire>(`${PATH_RUNS}${q}`);
    return normalizeRunList(raw);
  }

  /** `POST /chat/runs/{run_id}/delegate` — multi-agent coordination. */
  async delegateRun(runId: string, body: DelegationRequestBody): Promise<DelegationResponse> {
    return this.post<DelegationResponse>(chatRunDelegatePath(runId), body);
  }

  /** `GET /chat/runs/{run_id}/delegations` — sub-run ids for this parent run. */
  async listDelegations(runId: string): Promise<DelegationListResponse> {
    return this.fetch<DelegationListResponse>(chatRunDelegationsPath(runId));
  }

  /** `POST /chat/runs/{run_id}/delegations/pause` */
  async pauseDelegations(runId: string): Promise<DelegationMutationResponse> {
    return this.post<DelegationMutationResponse>(chatRunDelegationsPausePath(runId));
  }

  /** `POST /chat/runs/{run_id}/delegations/resume` */
  async resumeDelegations(runId: string): Promise<DelegationMutationResponse> {
    return this.post<DelegationMutationResponse>(chatRunDelegationsResumePath(runId));
  }

  // ─── Memory ─────────────────────────────────────────────────────────

  async memoryStore(entry: MemoryEntry): Promise<{ id: string }> {
    return this.post<{ id: string }>(PATH_MEMORY_STORE, entry);
  }

  async memorySearch(query: string, topK = 10): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>(PATH_MEMORY_SEARCH, { query, top_k: topK });
  }

  async memoryRetrieve(query: string, topK = 5): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>(PATH_MEMORY_RETRIEVE, { query, top_k: topK });
  }

  async memoryPurge(topic: string): Promise<void> {
    await this.post(PATH_MEMORY_PURGE, { topic });
  }

  // ─── Skills ─────────────────────────────────────────────────────────

  async listSkills(): Promise<SkillInfo[]> {
    const raw = await this.fetch<SkillListWire>(PATH_SKILLS);
    return raw.skills.map((s) => ({
      id: s.skill_id,
      name: s.skill_name,
      description: s.description ?? '',
      status: s.status ?? s.version ?? '',
    }));
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
  async getSkill(skillId: string, opts?: { version?: string }): Promise<SkillRecord> {
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
      method: 'POST',
      body: JSON.stringify(chatRequestToWire(request)),
      token: this.accessToken ?? undefined,
      headers: this.config.headers,
      onEvent: callbacks.onEvent,
      onStateChange: callbacks.onStateChange,
      onRawLine: callbacks.onRawLine,
      maxRetries: 0,
      signal: callbacks.signal,
    });

    client.connect().catch(() => {});
    return client;
  }

  // ─── §5.5 Edge callbacks & task leases ─────────────────────────────

  async postToolResult(
    body: ToolResultRequestBody,
    options?: { edgeExecutorId?: string },
  ): Promise<unknown> {
    const headers: Record<string, string> = {};
    if (options?.edgeExecutorId) {
      headers[ASTRA_EDGE_ID_HEADER] = options.edgeExecutorId;
    }
    return this.fetch(PATH_TOOLS_RESULT, {
      method: 'POST',
      body: JSON.stringify(body),
      headers,
    });
  }

  async postApprovalRespond(body: ApprovalRespondRequestBody): Promise<unknown> {
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
      method: 'POST',
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
      method: 'POST',
      body: JSON.stringify(body),
      headers,
    });
  }

  async getTaskLease(taskId: string): Promise<unknown> {
    return this.fetch(taskLeasePath(taskId));
  }

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
      method: 'POST',
      body: JSON.stringify(body),
      headers,
    });
  }

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
      method: 'POST',
      body: JSON.stringify(body),
      headers,
    });
  }

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
      method: 'POST',
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
  ) {
    super(`Astra API error ${status} on ${path}: ${body.slice(0, 200)}`);
    this.name = 'AstraApiError';
  }
}
