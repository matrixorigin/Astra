import type {
  AstraClientConfig,
  AuthResult,
  ChatRequest,
  MemoryEntry,
  MemorySearchResult,
  RunStatus,
  SessionAudit,
  SessionInfo,
  SkillInfo,
  StreamEvent,
  ConnectionState,
  UserInfo,
} from './types';
import { SSEClient } from './sse-client';

/**
 * Astra HTTP + SSE client for server communication.
 *
 * Handles authentication (JWT with auto-refresh), REST endpoints for
 * sessions/runs, and SSE streaming for chat responses.
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

  // ─── Auth ──────────────────────────────────────────────────────────

  /** Register a new user account. */
  async register(username: string, password: string): Promise<AuthResult> {
    const result = await this.post<AuthResult>('/api/auth/register', { username, password });
    this.accessToken = result.access_token;
    this.refreshTokenValue = result.refresh_token;
    return result;
  }

  /** Log in with username/password. Stores tokens automatically. */
  async login(username: string, password: string): Promise<AuthResult> {
    const result = await this.post<AuthResult>('/api/auth/login', { username, password });
    this.accessToken = result.access_token;
    this.refreshTokenValue = result.refresh_token;
    return result;
  }

  /** Log out and clear stored tokens. */
  async logout(): Promise<void> {
    try {
      await this.post('/api/auth/logout');
    } finally {
      this.accessToken = null;
      this.refreshTokenValue = null;
    }
  }

  /** Get the current authenticated user's info. */
  async getMe(): Promise<UserInfo> {
    return this.fetch<UserInfo>('/api/auth/me');
  }

  setTokens(accessToken: string, refreshToken?: string): void {
    this.accessToken = accessToken;
    if (refreshToken) this.refreshTokenValue = refreshToken;
  }

  private async tryRefreshToken(): Promise<boolean> {
    if (!this.refreshTokenValue) return false;
    try {
      const res = await fetch(`${this.config.baseUrl}/api/auth/refresh`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: this.refreshTokenValue }),
      });
      if (!res.ok) return false;
      const data = (await res.json()) as { access_token: string; refresh_token: string };
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

  private buildHeaders(): Record<string, string> {
    const headers: Record<string, string> = {
      'Content-Type': 'application/json',
      ...this.config.headers,
    };
    if (this.accessToken) {
      headers['Authorization'] = `Bearer ${this.accessToken}`;
    }
    return headers;
  }

  // ─── HTTP helpers ──────────────────────────────────────────────────

  async fetch<T>(path: string, init?: RequestInit): Promise<T> {
    let res = await fetch(`${this.config.baseUrl}${path}`, {
      ...init,
      headers: { ...this.buildHeaders(), ...init?.headers },
    });

    if (res.status === 401) {
      const refreshed = await this.tryRefreshToken();
      if (refreshed) {
        res = await fetch(`${this.config.baseUrl}${path}`, {
          ...init,
          headers: { ...this.buildHeaders(), ...init?.headers },
        });
      }
    }

    if (!res.ok) {
      const body = await res.text().catch(() => '');
      throw new AstraApiError(res.status, body, path);
    }

    return res.json() as Promise<T>;
  }

  async post<T>(path: string, body?: unknown): Promise<T> {
    return this.fetch<T>(path, {
      method: 'POST',
      body: body ? JSON.stringify(body) : undefined,
    });
  }

  // ─── Sessions ──────────────────────────────────────────────────────

  async createSession(): Promise<SessionInfo> {
    return this.post<SessionInfo>('/api/sessions');
  }

  async getSession(sessionId: string): Promise<SessionInfo> {
    return this.fetch<SessionInfo>(`/api/sessions/${sessionId}`);
  }

  async listSessions(): Promise<SessionInfo[]> {
    return this.fetch<SessionInfo[]>('/api/sessions');
  }

  async deleteSession(sessionId: string): Promise<void> {
    await this.fetch(`/api/sessions/${sessionId}`, { method: 'DELETE' });
  }

  async getSessionAudit(sessionId: string): Promise<SessionAudit> {
    return this.fetch<SessionAudit>(`/api/sessions/${sessionId}/audit`);
  }

  // ─── Runs ──────────────────────────────────────────────────────────

  async createRun(request: ChatRequest): Promise<RunStatus> {
    return this.post<RunStatus>('/api/runs', request);
  }

  async getRunStatus(runId: string): Promise<RunStatus> {
    return this.fetch<RunStatus>(`/api/runs/${runId}`);
  }

  async cancelRun(runId: string): Promise<void> {
    await this.post(`/api/runs/${runId}/cancel`);
  }

  async pauseRun(runId: string): Promise<void> {
    await this.post(`/api/runs/${runId}/pause`);
  }

  async resumeRun(runId: string): Promise<void> {
    await this.post(`/api/runs/${runId}/resume`);
  }

  async getRunEvents(runId: string, startIndex = 0): Promise<StreamEvent[]> {
    return this.fetch<StreamEvent[]>(`/api/runs/${runId}/events?start=${startIndex}`);
  }

  // ─── Memory ─────────────────────────────────────────────────────────

  async memoryStore(entry: MemoryEntry): Promise<{ id: string }> {
    return this.post<{ id: string }>('/api/memory/store', entry);
  }

  async memorySearch(query: string, topK = 10): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>('/api/memory/search', { query, top_k: topK });
  }

  async memoryRetrieve(query: string, topK = 5): Promise<MemorySearchResult[]> {
    return this.post<MemorySearchResult[]>('/api/memory/retrieve', { query, top_k: topK });
  }

  async memoryPurge(topic: string): Promise<void> {
    await this.post('/api/memory/purge', { topic });
  }

  // ─── Skills ─────────────────────────────────────────────────────────

  async listSkills(): Promise<SkillInfo[]> {
    return this.fetch<SkillInfo[]>('/api/skills');
  }

  // ─── Streaming ─────────────────────────────────────────────────────

  /**
   * Stream a chat message, receiving events as they arrive.
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
    const url = new URL(`${this.config.baseUrl}/api/chat/stream`);
    url.searchParams.set('message', request.message);
    if (request.sessionId) url.searchParams.set('session_id', request.sessionId);
    if (request.model) url.searchParams.set('model', request.model);

    const client = new SSEClient({
      url: url.toString(),
      token: this.accessToken ?? undefined,
      headers: this.config.headers,
      onEvent: callbacks.onEvent,
      onStateChange: callbacks.onStateChange,
      onRawLine: callbacks.onRawLine,
      maxRetries: 0, // Don't retry chat streams
      signal: callbacks.signal,
    });

    client.connect().catch(() => {});
    return client;
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
