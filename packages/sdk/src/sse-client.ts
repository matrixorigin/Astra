import type { StreamEvent, ConnectionState, SSEClientOptions } from './types';

/** Read Axum-style `{ detail }` or common `{ message, error }` from a failed fetch body. */
export async function readHttpErrorMessage(response: Response): Promise<string> {
  const statusLine = `${response.status} ${response.statusText}`.trim();
  try {
    const text = await response.text();
    if (!text?.trim()) return statusLine;
    try {
      const j = JSON.parse(text) as { detail?: string; message?: string; error?: string | { message?: string } };
      if (typeof j.detail === 'string' && j.detail) return j.detail;
      if (typeof j.message === 'string' && j.message) return j.message;
      if (typeof j.error === 'string' && j.error) return j.error;
      if (j.error && typeof j.error === 'object' && typeof j.error.message === 'string') {
        return j.error.message;
      }
    } catch {
      return text;
    }
    return text;
  } catch {
    return statusLine;
  }
}

/**
 * Parse a complete SSE response body (one or more `data: {json}\\n\\n` blocks) into stream events.
 * Used for buffered endpoints such as `GET /chat/runs/{id}/stream`.
 */
export function parseSseDataEvents(raw: string): StreamEvent[] {
  const events: StreamEvent[] = [];
  const blocks = raw.split(/\n\n+/);
  for (const block of blocks) {
    const lines = block.split('\n').filter((l) => l.length > 0);
    let data = '';
    for (const line of lines) {
      if (line.startsWith('data: ')) {
        data += line.slice(6);
      } else if (line.startsWith('data:')) {
        data += line.slice(5).trimStart();
      }
    }
    if (!data.trim()) continue;
    try {
      events.push(JSON.parse(data) as StreamEvent);
    } catch {
      // skip malformed
    }
  }
  return events;
}

function isTerminalEvent(event: StreamEvent): boolean {
  return (
    event.type === 'turn_complete' ||
    event.type === 'run_finished' ||
    event.type === 'run_cancelled' ||
    event.type === 'run_error' ||
    event.type === 'run_interrupted' ||
    event.type === 'run_paused' ||
    event.type === 'run_waiting' ||
    (event.type === 'error' && event.retryable !== true)
  );
}

/**
 * Fetch-based SSE client with automatic retry and custom auth headers.
 *
 * Uses `fetch()` + `ReadableStream` instead of the browser `EventSource` API
 * so that custom headers (Authorization, etc.) can be sent on the initial request.
 */
export class SSEClient {
  private options: Required<Pick<SSEClientOptions, 'url' | 'onEvent' | 'maxRetries' | 'retryDelayMs'>> &
    SSEClientOptions;
  private controller: AbortController | null = null;
  private retryCount = 0;
  private closed = false;
  private heartbeatTimer: ReturnType<typeof setTimeout> | null = null;
  private sawTerminalEvent = false;

  constructor(options: SSEClientOptions) {
    this.options = {
      maxRetries: 5,
      retryDelayMs: 2000,
      ...options,
    };
  }

  async connect(): Promise<void> {
    this.retryCount = 0;
    await this.connectAttempt();
  }

  private async connectAttempt(): Promise<void> {
    this.closed = false;
    this.sawTerminalEvent = false;
    this.options.onStateChange?.('connecting');

    this.controller = new AbortController();
    const linkedSignal = this.options.signal
      ? combineSignals(this.options.signal, this.controller.signal)
      : this.controller.signal;

    try {
      const headers: Record<string, string> = {
        Accept: 'text/event-stream',
        'Cache-Control': 'no-cache',
        ...this.options.headers,
      };
      if (this.options.token) {
        headers['Authorization'] = `Bearer ${this.options.token}`;
      }
      if (this.options.method === 'POST' && !headers['Content-Type']) {
        headers['Content-Type'] = 'application/json';
      }

      const response = await fetch(this.options.url, {
        method: this.options.method ?? 'GET',
        headers,
        body: this.options.body,
        signal: linkedSignal,
      });

      if (!response.ok) {
        if (this.options.decodeHttpError) {
          const event = await this.options.decodeHttpError(response);
          this.options.onStateChange?.('error');
          this.options.onEvent(event);
          return;
        }
        const detail = await readHttpErrorMessage(response);
        throw new Error(detail);
      }
      if (!response.body) {
        throw new Error('SSE response has no body');
      }

      this.options.onStateChange?.('connected');
      await this.readStream(response.body);

      if (!this.closed) {
        this.options.onStateChange?.('disconnected');
      }
    } catch (err) {
      if (this.closed || linkedSignal.aborted) return;
      const message = err instanceof Error ? err.message : 'Unknown error';
      this.options.onStateChange?.('error');
      this.options.onEvent({
        type: 'error',
        message: `Connection error: ${message}`,
        retryable: this.retryCount < this.options.maxRetries,
      } as StreamEvent);
      await this.maybeRetry();
    }
  }

  close(): void {
    this.closed = true;
    this.clearHeartbeatTimer();
    this.controller?.abort();
    this.controller = null;
    this.options.onStateChange?.('disconnected');
  }

  private async readStream(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';
    let streamFailed = false;
    let streamError: unknown;

    try {
      this.resetHeartbeatTimer();
      while (!this.closed) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });
        const parts = buffer.split('\n\n');
        buffer = parts.pop() ?? '';

        for (const part of parts) {
          this.processSSEChunk(part);
        }
      }
    } catch (error) {
      // Once the server has published a protocol terminal, a trailing socket
      // reset is transport noise rather than a second failure of the turn.
      // Preserve the successful terminal projection and only surface stream
      // errors that happen before it.
      if (!this.sawTerminalEvent) {
        streamFailed = true;
        streamError = error;
      }
    } finally {
      // Flush remaining buffer (handles events without trailing \n\n)
      buffer += decoder.decode();
      if (buffer.trim().length > 0) {
        this.processSSEChunk(buffer);
      }
      this.clearHeartbeatTimer();
      reader.releaseLock();
    }
    if (streamFailed) throw streamError;
    if (this.options.requireTerminalEvent && !this.sawTerminalEvent) {
      throw new Error(
        'SSE stream ended before a terminal event (run_finished, turn_complete, or interruption)',
      );
    }
  }

  private processSSEChunk(chunk: string): void {
    const lines = chunk.split('\n');
    let data = '';

    for (const line of lines) {
      this.options.onRawLine?.(line);

      if (line.startsWith('data: ')) {
        data += line.slice(6);
      } else if (line.startsWith('data:')) {
        data += line.slice(5);
      }
    }

    if (!data) return;

    try {
      const event = JSON.parse(data) as StreamEvent;
      this.resetHeartbeatTimer();
      this.sawTerminalEvent ||= isTerminalEvent(event);
      this.options.onEvent(event);
    } catch {
      // Ignore malformed JSON
    }
  }

  private resetHeartbeatTimer(): void {
    this.clearHeartbeatTimer();
    if (!this.options.heartbeatTimeoutMs || this.options.heartbeatTimeoutMs <= 0) return;
    this.heartbeatTimer = setTimeout(() => {
      // A terminal lifecycle event is authoritative. A provider/proxy that
      // keeps the HTTP body open after it has published that event must not
      // turn an otherwise completed turn into a retryable heartbeat error.
      if (this.closed || this.sawTerminalEvent) return;
      this.controller?.abort();
      this.options.onStateChange?.('error');
      this.options.onEvent({
        type: 'error',
        message: `Connection timed out after ${this.options.heartbeatTimeoutMs}ms without heartbeat`,
        retryable: true,
      } as StreamEvent);
    }, this.options.heartbeatTimeoutMs);
  }

  private clearHeartbeatTimer(): void {
    if (this.heartbeatTimer) {
      clearTimeout(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private async maybeRetry(): Promise<void> {
    this.retryCount++;
    if (this.closed || this.retryCount > this.options.maxRetries) return;

    const delay = this.options.retryDelayMs * Math.pow(1.5, this.retryCount - 1);
    await new Promise((r) => setTimeout(r, delay));

    if (!this.closed) {
      await this.connectAttempt();
    }
  }
}

function combineSignals(a: AbortSignal, b: AbortSignal): AbortSignal {
  const controller = new AbortController();
  const onAbort = () => controller.abort();
  a.addEventListener('abort', onAbort, { once: true });
  b.addEventListener('abort', onAbort, { once: true });
  if (a.aborted || b.aborted) controller.abort();
  return controller.signal;
}
