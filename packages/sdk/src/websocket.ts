import type { StreamEvent, ConnectionState } from './types';

export type AstraWebSocketOptions = {
  url: string;
  token?: string;
  protocols?: string[];
  onEvent: (event: StreamEvent) => void;
  onStateChange?: (state: ConnectionState) => void;
  reconnect?: boolean;
  maxReconnectAttempts?: number;
  reconnectDelayMs?: number;
};

export type ToolApproval = {
  callId: string;
  approved: boolean;
  reason?: string;
};

/**
 * WebSocket client for interactive Astra sessions.
 *
 * Supports bidirectional communication: receiving streaming events and
 * sending tool approvals, messages, and control signals.
 */
export class AstraWebSocket {
  private ws: WebSocket | null = null;
  private options: Required<Pick<AstraWebSocketOptions, 'reconnect' | 'maxReconnectAttempts' | 'reconnectDelayMs'>> &
    AstraWebSocketOptions;
  private reconnectAttempts = 0;
  private closed = false;

  constructor(options: AstraWebSocketOptions) {
    this.options = {
      reconnect: true,
      maxReconnectAttempts: 5,
      reconnectDelayMs: 2000,
      ...options,
    };
  }

  connect(): void {
    this.closed = false;
    this.options.onStateChange?.('connecting');

    const url = new URL(this.options.url);
    if (this.options.token) {
      url.searchParams.set('token', this.options.token);
    }

    this.ws = new WebSocket(url.toString(), this.options.protocols);

    this.ws.onopen = () => {
      this.reconnectAttempts = 0;
      this.options.onStateChange?.('connected');
    };

    this.ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data as string) as StreamEvent;
        this.options.onEvent(data);
      } catch {
        // Ignore malformed messages
      }
    };

    this.ws.onclose = () => {
      if (this.closed) {
        this.options.onStateChange?.('disconnected');
        return;
      }
      this.options.onStateChange?.('disconnected');
      this.maybeReconnect();
    };

    this.ws.onerror = () => {
      this.options.onStateChange?.('error');
    };
  }

  close(): void {
    this.closed = true;
    this.ws?.close();
    this.ws = null;
    this.options.onStateChange?.('disconnected');
  }

  sendMessage(content: string): void {
    this.send({ type: 'message', content });
  }

  approveToolCall(approval: ToolApproval): void {
    this.send({ type: 'tool_approval', ...approval });
  }

  cancelRun(): void {
    this.send({ type: 'cancel' });
  }

  private send(payload: unknown): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(payload));
    }
  }

  private maybeReconnect(): void {
    if (!this.options.reconnect || this.closed) return;
    if (this.reconnectAttempts >= this.options.maxReconnectAttempts) return;

    this.reconnectAttempts++;
    const delay = this.options.reconnectDelayMs * Math.pow(1.5, this.reconnectAttempts - 1);

    setTimeout(() => {
      if (!this.closed) {
        this.connect();
      }
    }, delay);
  }

  get readyState(): number {
    return this.ws?.readyState ?? WebSocket.CLOSED;
  }

  get isConnected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }
}
