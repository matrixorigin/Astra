import type { StreamEvent, StreamEventType, ConnectionState } from "./types";
import { modelSelectionToWire } from "./wire";
import { ASTRA_AGENT_INTERACTION_API_MAJOR } from "./paths";

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
  /** WebSocket URL (e.g. `ws://localhost:17001/chat/ws`). */
  url: string;
  /** JWT access token for authentication. */
  token: string;
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

export type UserPromptAnswer = {
  question: string;
  answers: string[];
  multi_select: boolean;
  annotation?: { notes?: string; preview?: string };
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
 * const ws = new AstraWebSocket({ url: 'ws://localhost:17001/chat/ws', token });
 * ws.on('tool_approval_request', (event) => {
 *   ws.approveToolCall({ callId: event.request_id, approved: true });
 * });
 * ws.on('text_delta', (event) => console.log(event.content));
 * await ws.connect();
 * ws.sendMessage('Hello!', { modelSelection: { offeringId: 'offer-gpt-4' } });
 * ```
 */
export class AstraWebSocket {
  private ws: WebSocket | null = null;
  private opts: Required<
    Pick<
      AstraWebSocketOptions,
      "reconnect" | "maxReconnectAttempts" | "reconnectDelayMs"
    >
  > &
    AstraWebSocketOptions;
  private reconnectAttempts = 0;
  private closed = false;
  // Replay the last event inclusively: one durable event can project to
  // multiple frames, and the socket can close between those frames.
  private replayEventIndex = 0;
  private seenReplayFrames = new Set<string>();
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  private listeners = new Map<string, Set<Listener<any>>>();

  // ── Public state ──
  sessionId: string | null = null;
  runId: string | null = null;
  connectionState: ConnectionState = "disconnected";

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
  on<K extends keyof AstraWSEventMap>(
    type: K,
    listener: Listener<AstraWSEventMap[K]>,
  ): this {
    if (!this.listeners.has(type)) {
      this.listeners.set(type, new Set());
    }
    this.listeners.get(type)!.add(listener);
    return this;
  }

  /** Unsubscribe from events. */
  off<K extends keyof AstraWSEventMap>(
    type: K,
    listener: Listener<AstraWSEventMap[K]>,
  ): this {
    this.listeners.get(type)?.delete(listener);
    return this;
  }

  private emit<K extends keyof AstraWSEventMap>(
    type: K,
    data: AstraWSEventMap[K],
  ): void {
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
   * Connect to the WebSocket server. Resolves after authentication and after
   * sending any active run's reattach request at its last durable cursor.
   */
  connect(): Promise<void> {
    if (!this.opts.token) {
      return Promise.reject(new Error("WebSocket authentication token is required"));
    }
    this.closed = false;
    this.setConnectionState("connecting");

    return new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(this.opts.url, this.opts.protocols);
      this.ws = socket;
      let authenticated = false;

      socket.onopen = () => {
        socket.send(JSON.stringify({
          type: "auth",
          token: this.opts.token.startsWith("Bearer ")
            ? this.opts.token
            : `Bearer ${this.opts.token}`,
          interaction_api_major: ASTRA_AGENT_INTERACTION_API_MAJOR,
        }));
      };

      socket.onmessage = (msg) => {
        try {
          const data = JSON.parse(msg.data as string) as {
            type: string;
            interaction_api_major?: string;
            message?: string;
          };
          if (!authenticated) {
            if (data.type === "auth_error") {
              this.closed = true;
              socket.close();
              reject(new Error(data.message ?? "WebSocket authentication failed"));
              return;
            }
            if (data.type !== "auth_ok") return;
            if (data.interaction_api_major !== ASTRA_AGENT_INTERACTION_API_MAJOR) {
              this.closed = true;
              socket.close();
              reject(new Error("WebSocket interaction contract mismatch"));
              return;
            }
            authenticated = true;
            if (this.runId) {
              socket.send(JSON.stringify({
                type: "attach_run",
                run_id: this.runId,
                last_index: this.replayEventIndex,
              }));
            }
            this.reconnectAttempts = 0;
            this.setConnectionState("connected");
            resolve();
            return;
          }
          this.processEvent(data as StreamEvent);
        } catch {
          // Ignore malformed messages
        }
      };

      socket.onclose = () => {
        if (this.ws !== socket) return;
        if (!authenticated) reject(new Error("WebSocket closed before authentication"));
        if (this.closed) {
          this.setConnectionState("disconnected");
          return;
        }
        this.setConnectionState("disconnected");
        this.maybeReconnect();
      };

      socket.onerror = () => {
        this.setConnectionState("error");
        if (!authenticated) {
          reject(new Error("WebSocket connection failed"));
        }
      };
    });
  }

  /** Disconnect and stop reconnection. */
  close(): void {
    this.closed = true;
    this.ws?.close();
    this.ws = null;
    this.setConnectionState("disconnected");
  }

  // ─── Outgoing messages ────────────────────────────────────────────

  /** Send a chat message to the agent. */
  sendMessage(
    content: string,
    options: {
      sessionId?: string;
      modelSelection: { offeringId: string };
    },
  ): void {
    this.send({
      type: "message",
      content,
      ...(options.sessionId && { session_id: options.sessionId }),
      model_selection: modelSelectionToWire(options?.modelSelection),
    });
  }

  /** Respond to a tool approval request. */
  approveToolCall(approval: ToolApproval): void {
    this.send({
      type: "tool_approval",
      request_id: approval.callId,
      approved: approval.approved,
      ...(approval.reason && { reason: approval.reason }),
    });
  }

  /** Cancel the currently running agent run. */
  cancelRun(runId?: string): void {
    this.send({ type: "cancel_run", ...(runId && { run_id: runId }) });
  }

  /** Pause the currently running agent run. */
  pauseRun(runId?: string): void {
    this.send({ type: "pause_run", ...(runId && { run_id: runId }) });
  }

  /** Resume a paused agent run. */
  resumeRun(runId?: string): void {
    this.send({ type: "resume_run", ...(runId && { run_id: runId }) });
  }

  /** Attach to an owned run, replaying durable events from lastIndex. */
  attachRun(runId: string, lastIndex = 0): void {
    if (!runId) throw new Error("runId is required");
    if (!Number.isSafeInteger(lastIndex) || lastIndex < 0 || lastIndex > 0xffffffff) {
      throw new Error("lastIndex must be a nonnegative 32-bit integer");
    }
    if (this.isConnected && this.runId) {
      throw new Error("a run is already attached");
    }
    this.runId = runId;
    this.replayEventIndex = lastIndex;
    this.seenReplayFrames.clear();
    this.send({ type: "attach_run", run_id: runId, last_index: lastIndex });
  }

  /** Resolve an ask_user prompt on the attached run. */
  respondToUserPrompt(requestId: string, answers: UserPromptAnswer[]): void {
    this.send({ type: "user_prompt", request_id: requestId, answers: { answers } });
  }

  /** Cancel an ask_user prompt on the attached run. */
  cancelUserPrompt(requestId: string): void {
    this.send({ type: "user_prompt", request_id: requestId, cancelled: true });
  }

  // ─── Getters ──────────────────────────────────────────────────────

  get readyState(): number {
    return this.ws?.readyState ?? WebSocket.CLOSED;
  }

  get isConnected(): boolean {
    return this.connectionState === "connected" && this.ws?.readyState === WebSocket.OPEN;
  }

  // ─── Internals ────────────────────────────────────────────────────

  private send(payload: unknown): void {
    if (this.isConnected) {
      this.ws?.send(JSON.stringify(payload));
    }
  }

  private setConnectionState(state: ConnectionState): void {
    this.connectionState = state;
    // Legacy callback
    this.opts.onStateChange?.(state);
    // Event emitter
    this.emit("stateChange", state);
  }

  private processEvent(event: StreamEvent): void {
    // Track session/run state from events
    if (event.type === "session_info" && "session_id" in event) {
      this.sessionId = (event as { session_id: string }).session_id;
      const runId = (event as { run_id?: string }).run_id;
      if (runId) {
        if (runId !== this.runId) {
          this.replayEventIndex = 0;
          this.seenReplayFrames.clear();
        }
        this.runId = runId;
      }
    }
    if (event.type === "run_started" && "run_id" in event) {
      const runId = (event as { run_id: string }).run_id;
      if (runId !== this.runId) {
        this.replayEventIndex = 0;
        this.seenReplayFrames.clear();
      }
      this.runId = runId;
    }
    if (typeof event.index === "number" && Number.isSafeInteger(event.index)) {
      if (event.index < this.replayEventIndex) return;
      if (event.index > this.replayEventIndex) {
        this.replayEventIndex = event.index;
        this.seenReplayFrames.clear();
      }
      const frame = JSON.stringify(event);
      if (this.seenReplayFrames.has(frame)) return;
      this.seenReplayFrames.add(frame);
    }
    if (event.type === "run_finished" || event.type === "run_cancelled") {
      this.runId = null;
      this.replayEventIndex = 0;
      this.seenReplayFrames.clear();
    }

    // Legacy callback
    this.opts.onEvent?.(event);
    // Emit to generic 'event' listeners
    this.emit("event", event);
    // Emit to type-specific listeners (e.g. 'text_delta')
    this.emit(event.type as keyof AstraWSEventMap, event);
  }

  private maybeReconnect(): void {
    if (!this.opts.reconnect || this.closed) return;
    if (this.reconnectAttempts >= this.opts.maxReconnectAttempts) return;

    this.reconnectAttempts++;
    const delay =
      this.opts.reconnectDelayMs * Math.pow(1.5, this.reconnectAttempts - 1);

    setTimeout(() => {
      if (!this.closed) {
        this.connect().catch(() => {
          // Reconnect failures handled by onclose → maybeReconnect cycle
        });
      }
    }, delay);
  }
}
