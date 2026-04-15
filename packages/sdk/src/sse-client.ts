import type { StreamEvent, ConnectionState, SSEClientOptions } from './types';

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
        throw new Error(`SSE connection failed: ${response.status} ${response.statusText}`);
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
    this.controller?.abort();
    this.controller = null;
    this.options.onStateChange?.('disconnected');
  }

  private async readStream(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';

    try {
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
    } finally {
      // Flush remaining buffer (handles events without trailing \n\n)
      buffer += decoder.decode();
      if (buffer.trim().length > 0) {
        this.processSSEChunk(buffer);
      }
      reader.releaseLock();
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
      this.options.onEvent(event);
    } catch {
      // Ignore malformed JSON
    }
  }

  private async maybeRetry(): Promise<void> {
    this.retryCount++;
    if (this.closed || this.retryCount > this.options.maxRetries) return;

    const delay = this.options.retryDelayMs * Math.pow(1.5, this.retryCount - 1);
    await new Promise((r) => setTimeout(r, delay));

    if (!this.closed) {
      await this.connect();
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
