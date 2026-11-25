import type { StreamEvent, ConnectionState, SSEClientOptions } from './types';
import { headersInitToRecord } from './http';

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
 * Parse a complete SSE response body into stream events (LF, CRLF, or CR lines).
 * Used for buffered endpoints such as `GET /chat/runs/{id}/stream`.
 */
export function parseSseDataEvents(raw: string, requireCompletion = false): StreamEvent[] {
  const events: StreamEvent[] = [];
  let complete = false;
  const parser = new SseDataParser((data) => {
    if (complete) return;
    if (data === "[DONE]") { complete = true; return; }
    const event = parseSseEvent(data);
    if (event === undefined) throw new TypeError('Invalid SSE event JSON');
    if (!event || typeof event !== 'object' || Array.isArray(event) ||
        typeof event.type !== 'string') throw new TypeError('Invalid SSE event envelope');
    events.push(event);
  });
  parser.push(raw);
  parser.finish();
  // The server may end an unsuccessful replay with an explicit error frame.
  if (requireCompletion && !complete && events.at(-1)?.type !== "error") {
    throw new Error("SSE stream ended before its completion marker [DONE]");
  }
  return events;
}

function parseSseEvent(data: string): StreamEvent | undefined {
  try {
    return JSON.parse(data) as StreamEvent;
  } catch {
    // Ignore malformed JSON.
    return undefined;
  }
}

/** Shared incremental line/data framing for buffered and live SSE consumers. */
class SseDataParser {
  private line = '';
  private data: string[] = [];
  private skipLf = false;
  private atStart = true;

  constructor(
    private onData: (data: string) => void,
    private onRawLine?: (line: string) => void,
  ) {}

  push(text: string): void {
    if (text.length === 0) return;
    if (this.atStart) {
      this.atStart = false;
      if (text.startsWith('\uFEFF')) text = text.slice(1);
    }
    let start = 0;
    for (let i = 0; i < text.length; i++) {
      const char = text[i];
      if (this.skipLf) {
        this.skipLf = false;
        if (char === '\n') {
          start = i + 1;
          continue;
        }
      }
      if (char === '\r' || char === '\n') {
        this.processLine(this.line + text.slice(start, i));
        this.line = '';
        this.skipLf = char === '\r';
        start = i + 1;
      }
    }
    this.line += text.slice(start);
  }

  finish(): void {
    if (this.line.length > 0) this.processLine(this.line);
    this.line = '';
    // Preserve the SDK's existing tolerance for a final event without a blank line.
    this.dispatch();
  }

  private processLine(line: string): void {
    this.onRawLine?.(line);
    if (line === '') {
      this.dispatch();
      return;
    }
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    if (field !== 'data') return;
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    this.data.push(value);
  }

  private dispatch(): void {
    if (this.data.length === 0) return;
    const data = this.data.join('\n');
    this.data = [];
    this.onData(data);
  }
}

/** A diagnostic or unstamped fallback cannot select the durable run outcome. */
export function isRunTerminalStatus(value: unknown): value is string {
  return typeof value === "string" && ["completed", "failed", "cancelled", "paused", "delegated"].includes(value);
}

export function isOwnedRunFinishedEvent<T>(value: T): value is T & {
  type: "run_finished"; run_id: string; owner_generation: number; status: string;
} {
  if (!value || typeof value !== "object") return false;
  const event = value as Record<string, unknown>;
  return event.type === "run_finished" && typeof event.run_id === "string" && !!event.run_id.trim()
    && typeof event.owner_generation === "number" && Number.isSafeInteger(event.owner_generation)
    && event.owner_generation >= 0 && isRunTerminalStatus(event.status);
}

/** These observations precede final publication; they do not finish the response. */
export function isRunFinalizationHint(event: { type?: unknown; execution_live?: unknown }): boolean {
  return event.type === "run_interrupted"
    || event.type === "run_waiting" && event.execution_live === false;
}

/** Durable owner receipts replay their journaled answer without a live turn_complete. */
export function requiresRunCompletionProjection(event: { status?: unknown; index?: unknown }): boolean {
  return (event.status === "completed" || event.status === "paused")
    && !(typeof event.index === "number" && Number.isSafeInteger(event.index) && event.index >= 0);
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
  private heartbeatController: AbortController | null = null;
  private retryCount = 0;
  private closed = false;
  private heartbeatTimer: ReturnType<typeof setTimeout> | null = null;
  private sawStreamCompletion = false;

  constructor(options: SSEClientOptions) {
    this.options = {
      maxRetries: 5,
      retryDelayMs: 2000,
      ...options,
    };
  }

  async connect(): Promise<void> {
    this.closed = false;
    this.retryCount = 0;
    await this.connectAttempt();
  }

  private async connectAttempt(): Promise<void> {
    if (this.closed || this.options.signal?.aborted) return;
    this.sawStreamCompletion = false;
    this.options.onStateChange?.('connecting');
    if (this.closed || this.options.signal?.aborted) return;

    this.controller = new AbortController();
    const linked = this.options.signal
      ? combineSignals(this.options.signal, this.controller.signal)
      : { signal: this.controller.signal, dispose: () => {} };
    const linkedSignal = linked.signal;
    // Heartbeat aborts only the stalled fetch, not the cancellable retry wait.
    const heartbeatController = new AbortController();
    this.heartbeatController = heartbeatController;
    const fetchSignal = combineSignals(linkedSignal, heartbeatController.signal);
    let retry = false;
    let response: Response | undefined;

    try {
      let headers = headersInitToRecord({
        Accept: 'text/event-stream',
        'Cache-Control': 'no-cache',
      }, this.options.headers);
      if (this.options.token) {
        headers = headersInitToRecord(headers, { Authorization: `Bearer ${this.options.token}` });
      }
      if (this.options.method === 'POST' && !new Headers(headers).has('Content-Type')) {
        headers = headersInitToRecord(headers, { 'Content-Type': 'application/json' });
      }

      response = await (this.options.transport ?? fetch)(this.options.url, {
        method: this.options.method ?? 'GET',
        headers,
        body: this.options.body,
        signal: fetchSignal.signal,
      });

      if (this.closed || linkedSignal.aborted) return;
      if (!response.ok) {
        if (this.options.decodeHttpError) {
          const event = await this.options.decodeHttpError(response);
          if (this.closed || linkedSignal.aborted) return;
          this.options.onStateChange?.('error');
          if (this.closed || linkedSignal.aborted) return;
          this.options.onEvent(event);
          return;
        }
        const detail = await readHttpErrorMessage(response);
        if (this.closed || linkedSignal.aborted) return;
        if (response.status >= 400 && response.status < 500 && response.status !== 408
          && (this.options.method === 'POST' || response.status !== 429)) {
          this.options.onStateChange?.('error');
          if (this.closed || linkedSignal.aborted) return;
          this.options.onEvent({
            type: 'error', message: detail, http_status: response.status, retryable: false,
          });
          return;
        }
        throw new Error(detail);
      }
      if (!response.body) {
        throw new Error('SSE response has no body');
      }

      this.options.onStateChange?.('connected');
      await this.readStream(response.body, linkedSignal, heartbeatController);
      if (linkedSignal.aborted) return;
      if (heartbeatController.signal.aborted && !this.sawStreamCompletion) {
        throw new Error('Heartbeat timeout');
      }

      if (!this.closed) {
        this.options.onStateChange?.('disconnected');
      }
    } catch (err) {
      if (this.closed || linkedSignal.aborted || this.sawStreamCompletion) return;
      const message = err instanceof Error ? err.message : 'Unknown error';
      this.options.onStateChange?.('error');
      if (this.closed || linkedSignal.aborted) return;
      this.options.onEvent({
        type: 'error',
        ...(response && !response.ok ? { http_status: response.status } : {}),
        message: heartbeatController.signal.aborted
          ? `Connection timed out after ${this.options.heartbeatTimeoutMs}ms without heartbeat`
          : `Connection error: ${message}`,
        code: 'stream_observation_interrupted',
        error_kind: 'stream_transport',
        retryable: this.options.method !== 'POST' && this.retryCount < this.options.maxRetries,
      } as StreamEvent);
      retry = await this.maybeRetry(linkedSignal);
    } finally {
      fetchSignal.dispose();
      linked.dispose();
    }
    if (retry) await this.connectAttempt();
  }

  close(): void {
    this.closed = true;
    this.clearHeartbeatTimer();
    this.controller?.abort();
    this.controller = null;
    this.heartbeatController = null;
    this.options.onStateChange?.('disconnected');
  }

  private async readStream(
    body: ReadableStream<Uint8Array>, signal: AbortSignal, heartbeatController: AbortController,
  ): Promise<void> {
    if (signal.aborted) return;
    const reader = body.getReader();
    // Preserve the BOM here; SseDataParser owns removing exactly one leading BOM.
    const decoder = new TextDecoder('utf-8', { ignoreBOM: true });
    const parser = new SseDataParser(
      (data) => { if (!signal.aborted) this.processSSEData(data); },
      (line) => {
        if (!this.closed && !this.sawStreamCompletion && !this.options.signal?.aborted) {
          this.options.onRawLine?.(line);
        }
      },
    );
    let streamFailed = false;
    let streamError: unknown;

    try {
      this.resetHeartbeatTimer();
      while (!this.closed && !signal.aborted && !this.sawStreamCompletion) {
        const { done, value } = await reader.read();
        if (done) break;

        parser.push(decoder.decode(value, { stream: true }));
      }
    } catch (error) {
      // Once the server has closed observation with [DONE], a trailing socket
      // reset cannot replace the already selected observation closure.
      // Execution outcomes remain with the event consumer.
      if (!this.sawStreamCompletion) {
        streamFailed = true;
        streamError = error;
      }
    } finally {
      // Only clean EOF may complete an unterminated tail. A reset or abort
      // cannot promote partial bytes to a completion marker or owner projection.
      if (!streamFailed && !signal.aborted && !this.closed) {
        parser.push(decoder.decode());
        parser.finish();
      }
      // close() already cleared the cancelled attempt's timer. A replacement
      // connection may now own the shared timer.
      if (this.heartbeatController === heartbeatController) this.clearHeartbeatTimer();
      if (this.sawStreamCompletion) {
        // DONE owns observation closure; release an otherwise open response body.
        // A trailing reset/cancel error cannot replace the selected closure.
        await reader.cancel().catch(() => {});
      }
      reader.releaseLock();
    }
    if (signal.aborted) return;
    if (streamFailed && !this.sawStreamCompletion) throw streamError;
    if (this.options.requireTerminalEvent && !this.sawStreamCompletion) {
      throw new Error(
        'SSE stream ended before its completion marker [DONE]',
      );
    }
  }

  private processSSEData(data: string): void {
    // Cancellation can occur in a callback while the parser still holds more
    // events from the same network chunk, including its final buffered event.
    if (this.closed || this.sawStreamCompletion || this.options.signal?.aborted) return;
    // Framing owns observation completion; run/child outcome belongs to the consumer.
    if (data === "[DONE]") {
      this.sawStreamCompletion = true;
      return;
    }
    const event = parseSseEvent(data);
    if (event === undefined) return;
    try {
      this.resetHeartbeatTimer();
      this.options.onEvent(event);
    } catch {
      // Preserve the existing tolerance for invalid events or callback errors.
    }
  }

  private resetHeartbeatTimer(): void {
    this.clearHeartbeatTimer();
    if (!this.options.heartbeatTimeoutMs || this.options.heartbeatTimeoutMs <= 0) return;
    this.heartbeatTimer = setTimeout(() => {
      // A completed SSE observation is authoritative. A provider/proxy that
      // keeps the HTTP body open after that marker must not invent another
      // observation error.
      if (this.closed || this.sawStreamCompletion) return;
      this.heartbeatController?.abort();
    }, this.options.heartbeatTimeoutMs);
  }

  private clearHeartbeatTimer(): void {
    if (this.heartbeatTimer) {
      clearTimeout(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  private async maybeRetry(signal: AbortSignal): Promise<boolean> {
    this.retryCount++;
    if (this.closed || signal.aborted || this.options.method === 'POST' || this.retryCount > this.options.maxRetries) return false;

    const delay = this.options.retryDelayMs * Math.pow(1.5, this.retryCount - 1);
    await new Promise<void>((resolve) => {
      const finish = () => {
        clearTimeout(timer);
        signal.removeEventListener('abort', finish);
        resolve();
      };
      const timer = setTimeout(finish, delay);
      signal.addEventListener('abort', finish, { once: true });
      if (signal.aborted) finish();
    });

    return !this.closed && !signal.aborted;
  }
}

function combineSignals(a: AbortSignal, b: AbortSignal): {
  signal: AbortSignal;
  dispose: () => void;
} {
  const controller = new AbortController();
  const onAbort = () => controller.abort();
  a.addEventListener('abort', onAbort, { once: true });
  b.addEventListener('abort', onAbort, { once: true });
  if (a.aborted || b.aborted) controller.abort();
  return {
    signal: controller.signal,
    dispose: () => {
      a.removeEventListener('abort', onAbort);
      b.removeEventListener('abort', onAbort);
    },
  };
}
