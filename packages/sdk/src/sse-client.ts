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
export function parseSseDataEvents(raw: string): StreamEvent[] {
  const events: StreamEvent[] = [];
  const parser = new SseDataParser((data) => {
    const event = parseSseEvent(data);
    if (event !== undefined) events.push(event);
  });
  parser.push(raw);
  parser.finish();
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
  private heartbeatController: AbortController | null = null;
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
    this.closed = false;
    this.retryCount = 0;
    await this.connectAttempt();
  }

  private async connectAttempt(): Promise<void> {
    if (this.closed || this.options.signal?.aborted) return;
    this.sawTerminalEvent = false;
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

      const response = await (this.options.transport ?? fetch)(this.options.url, {
        method: this.options.method ?? 'GET',
        headers,
        body: this.options.body,
        signal: fetchSignal.signal,
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
      if (heartbeatController.signal.aborted && !this.sawTerminalEvent) {
        throw new Error('Heartbeat timeout');
      }

      if (!this.closed) {
        this.options.onStateChange?.('disconnected');
      }
    } catch (err) {
      if (this.closed || linkedSignal.aborted || this.sawTerminalEvent) return;
      const message = err instanceof Error ? err.message : 'Unknown error';
      this.options.onStateChange?.('error');
      this.options.onEvent({
        type: 'error',
        message: heartbeatController.signal.aborted
          ? `Connection timed out after ${this.options.heartbeatTimeoutMs}ms without heartbeat`
          : `Connection error: ${message}`,
        retryable: this.retryCount < this.options.maxRetries,
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

  private async readStream(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    // Preserve the BOM here; SseDataParser owns removing exactly one leading BOM.
    const decoder = new TextDecoder('utf-8', { ignoreBOM: true });
    const parser = new SseDataParser(
      (data) => this.processSSEData(data),
      this.options.onRawLine,
    );
    let streamFailed = false;
    let streamError: unknown;

    try {
      this.resetHeartbeatTimer();
      while (!this.closed) {
        const { done, value } = await reader.read();
        if (done) break;

        parser.push(decoder.decode(value, { stream: true }));
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
      parser.push(decoder.decode());
      parser.finish();
      this.clearHeartbeatTimer();
      reader.releaseLock();
    }
    if (streamFailed && !this.sawTerminalEvent) throw streamError;
    if (this.options.requireTerminalEvent && !this.sawTerminalEvent) {
      throw new Error(
        'SSE stream ended before a terminal event (run_finished, turn_complete, or interruption)',
      );
    }
  }

  private processSSEData(data: string): void {
    // Cancellation can occur in a callback while the parser still holds more
    // events from the same network chunk, including its final buffered event.
    if (this.closed || this.options.signal?.aborted) return;
    const event = parseSseEvent(data);
    if (event === undefined) return;
    try {
      this.resetHeartbeatTimer();
      this.sawTerminalEvent ||= isTerminalEvent(event);
      this.options.onEvent(event);
    } catch {
      // Preserve the existing tolerance for invalid events or callback errors.
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
    if (this.closed || signal.aborted || this.retryCount > this.options.maxRetries) return false;

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
