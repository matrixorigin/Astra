import type { StreamEvent, StreamEventType, ConnectionState } from './types';

// ─── Event Emitter Types ────────────────────────────────────────────

export type AstraWSEventMap = {
  /** Fired for every stream event from the server. */
  event: StreamEvent;
  /** Fired when connection state changes. */
  stateChange: ConnectionState;
} & {
  /** Fired for a specific stream event type (e.g. 'text_delta'). */
  [K in StreamEventType]: StreamEvent;
};

type Listener<T> = (data: T) => void;

// ─── Options ────────────────────────────────────────────────────────

export type AstraWebSocketOptions = {
  /** WebSocket URL (e.g. `ws://localhost:8000/api/chat/ws`). */
  url: string;
  /** JWT access token for authentication. */
  token?: string;
  /** WebSocket sub-protocols. */
  protocols?: string[];

  // ── Legacy callback (backward compat) ──
  onEvent?: (event: StreamEvent) => void;
  onStateChange?: (state: ConnectionState) => void;

  // ── Reconnection ──
  reconnect?: boolean;
  maxReconnectAttempts?: number;
  reconnectDelayMs?: number;
};

export type ToolApproval = {
  callId: string;
  approved: boolean;
  reason?: string;
};

// ─── AstraWebSocket ─────────────────────────────────────────────────

/**
 * WebSocket client for interactive Astra sessions.
 *
 * Supports bidirectional communication: receiving streaming events and
 * sending tool approvals, messages, and control signals.
 *
 * @example Event emitter pattern
 * ```ts
 * const ws = new AstraWebSocket({ url: 'ws://localhost:8000/api/chat/ws', token });
 * ws.on('tool_approval_request', (event) => {
 *   ws.approveToolCall({ callId: event.request_id, approved: true });
 * });
 * ws.on('text_delta', (event) => console.log(event.content));
 * await ws.connect();
 * ws.sendMessage('Hello!');
 * ```
 */
export class AstraWebSocket {
  private ws: WebSocket | null = null;
  private opts: Required<
    Pick<AstraWebSocketOptions, 'reconnect' | 'maxReconnectAttempts' | 'reconnectDelayMs'>
  > &
    AstraWebSocketOptions;
  private reconnectAttempts = 0;
  private closed = false;
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private listeners = new Map<string, Set<Listener<any>>>();

  // ── Public state ──
  sessionId: string | null = null;
  runId: string | null = null;
  connectionState: ConnectionState = 'disconnected';

  constructor(options: AstraWebSocketOptions) {
    this.opts = {
      reconnect: true,
      maxReconnectAttempts: 5,
      reconnectDelayMs: 2000,
      ...options,
    };
  }

  // ─── Event emitter ────────────────────────────────────────────────

  /**
   * Subscribe to events.
   *
   * - `'event'` — all stream events
   * - `'stateChange'` — connection state changes
   * - Any `StreamEventType` (e.g. `'text_delta'`, `'tool_approval_request'`)
   */
  on<K extends keyof AstraWSEventMap>(type: K, listener: Listener<AstraWSEventMap[K]>): this {
    if (!this.listeners.has(type)) {
      this.listeners.set(type, new Set());
    }
    this.listeners.get(type)!.add(listener);
    return this;
  }

  /** Unsubscribe from events. */
  off<K extends keyof AstraWSEventMap>(type: K, listener: Listener<AstraWSEventMap[K]>): this {
    this.listeners.get(type)?.delete(listener);
    return this;
  }

  private emit<K extends keyof AstraWSEventMap>(type: K, data: AstraWSEventMap[K]): void {
    this.listeners.get(type)?.forEach((fn) => {
      try {
        fn(data);
      } catch {
        // Don't let listener errors break the stream
      }
    });
  }

  // ─── Connection ───────────────────────────────────────────────────

  /**
   * Connect to the WebSocket server. Resolves when the connection is open
   * (or rejects on immediate failure).
   */
  connect(): Promise<void> {
    this.closed = false;
    this.setConnectionState('connecting');

    return new Promise<void>((resolve, reject) => {
      const url = new URL(this.opts.url);
      if (this.opts.token) {
        url.searchParams.set('token', this.opts.token);
      }

      this.ws = new WebSocket(url.toString(), this.opts.protocols);

      this.ws.onopen = () => {
        this.reconnectAttempts = 0;
        this.setConnectionState('connected');
        resolve();
      };

      this.ws.onmessage = (msg) => {
        try {
          const data = JSON.parse(msg.data as string) as StreamEvent;
          this.processEvent(data);
        } catch {
          // Ignore malformed messages
        }
      };

      this.ws.onclose = () => {
        if (this.closed) {
          this.setConnectionState('disconnected');
          return;
        }
        this.setConnectionState('disconnected');
        this.maybeReconnect();
      };

      this.ws.onerror = () => {
        this.setConnectionState('error');
        // Only reject on initial connect; reconnects don't have a promise
        if (this.reconnectAttempts === 0) {
          reject(new Error('WebSocket connection failed'));
        }
      };
    });
  }

  /** Disconnect and stop reconnection. */
  close(): void {
    this.closed = true;
    this.ws?.close();
    this.ws = null;
    this.setConnectionState('disconnected');
  }

  // ─── Outgoing messages ────────────────────────────────────────────

  /** Send a chat message to the agent. */
  sendMessage(content: string, options?: { sessionId?: string; model?: string }): void {
    this.send({
      type: 'message',
      content,
      ...(options?.sessionId && { session_id: options.sessionId }),
      ...(options?.model && { model: options.model }),
    });
  }

  /** Respond to a tool approval request. */
  approveToolCall(approval: ToolApproval): void {
    this.send({
      type: 'tool_approval',
      request_id: approval.callId,
      approved: approval.approved,
      ...(approval.reason && { reason: approval.reason }),
    });
  }

  /** Cancel the currently running agent run. */
  cancelRun(runId?: string): void {
    this.send({ type: 'cancel_run', ...(runId && { run_id: runId }) });
  }

  /** Pause the currently running agent run. */
  pauseRun(runId?: string): void {
    this.send({ type: 'pause_run', ...(runId && { run_id: runId }) });
  }

  /** Resume a paused agent run. */
  resumeRun(runId?: string): void {
    this.send({ type: 'resume_run', ...(runId && { run_id: runId }) });
  }

  // ─── Getters ──────────────────────────────────────────────────────

  get readyState(): number {
    return this.ws?.readyState ?? WebSocket.CLOSED;
  }

  get isConnected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  // ─── Internals ────────────────────────────────────────────────────

  private send(payload: unknown): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(payload));
    }
  }

  private setConnectionState(state: ConnectionState): void {
    this.connectionState = state;
    // Legacy callback
    this.opts.onStateChange?.(state);
    // Event emitter
    this.emit('stateChange', state);
  }

  private processEvent(event: StreamEvent): void {
    // Track session/run state from events
    if (event.type === 'session_info' && 'session_id' in event) {
      this.sessionId = (event as { session_id: string }).session_id;
    }
    if (event.type === 'run_started' && 'run_id' in event) {
      this.runId = (event as { run_id: string }).run_id;
    }
    if (event.type === 'run_finished' || event.type === 'run_cancelled') {
      this.runId = null;
    }

    // Legacy callback
    this.opts.onEvent?.(event);
    // Emit to generic 'event' listeners
    this.emit('event', event);
    // Emit to type-specific listeners (e.g. 'text_delta')
    this.emit(event.type as keyof AstraWSEventMap, event);
  }

  private maybeReconnect(): void {
    if (!this.opts.reconnect || this.closed) return;
    if (this.reconnectAttempts >= this.opts.maxReconnectAttempts) return;

    this.reconnectAttempts++;
    const delay = this.opts.reconnectDelayMs * Math.pow(1.5, this.reconnectAttempts - 1);

    setTimeout(() => {
      if (!this.closed) {
        this.connect().catch(() => {
          // Reconnect failures handled by onclose → maybeReconnect cycle
        });
      }
    }, delay);
  }
}
