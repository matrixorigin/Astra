import type { StreamEvent, ConnectionState } from './types';

export type SSEClientOptions = {
  url: string;
  token?: string;
  onEvent: (event: StreamEvent) => void;
  onStateChange: (state: ConnectionState) => void;
  onRawLine?: (line: string) => void;
  maxRetries?: number;
  retryDelayMs?: number;
};

/**
 * Minimal SSE client using `fetch` + `ReadableStream`.
 * Browser `EventSource` doesn't support custom headers (Authorization),
 * so we use fetch with streaming body instead.
 */
export class SSEClient {
  private controller: AbortController | null = null;
  private retryCount = 0;
  private closed = false;
  private readonly options: Required<
    Pick<SSEClientOptions, 'maxRetries' | 'retryDelayMs'>
  > &
    SSEClientOptions;

  constructor(options: SSEClientOptions) {
    this.options = {
      maxRetries: 5,
      retryDelayMs: 2000,
      ...options,
    };
  }

  async connect(): Promise<void> {
    if (this.closed) return;

    this.options.onStateChange('connecting');
    this.controller = new AbortController();

    try {
      const response = await fetch(this.options.url, {
        headers: this.options.token
          ? {
              Authorization: `Bearer ${this.options.token}`,
              Accept: 'text/event-stream',
            }
          : {
              Accept: 'text/event-stream',
            },
        signal: this.controller.signal,
      });

      if (!response.ok) {
        throw new Error(`SSE connection failed: ${response.status} ${response.statusText}`);
      }

      if (!response.body) {
        throw new Error('SSE response has no body');
      }

      this.options.onStateChange('connected');
      this.retryCount = 0;

      await this.readStream(response.body);
    } catch (err) {
      if (this.closed) return;

      const message = err instanceof Error ? err.message : 'Unknown error';

      // AbortError means we closed intentionally
      if (err instanceof DOMException && err.name === 'AbortError') return;

      this.options.onStateChange('error');
      this.options.onEvent({
        type: 'error',
        message: `Connection error: ${message}`,
        retryable: true,
      });

      await this.maybeRetry();
    }
  }

  close(): void {
    this.closed = true;
    this.controller?.abort();
    this.controller = null;
    this.options.onStateChange('disconnected');
  }

  private async readStream(body: ReadableStream<Uint8Array>): Promise<void> {
    const reader = body.getReader();
    const decoder = new TextDecoder();
    let buffer = '';

    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;

        buffer += decoder.decode(value, { stream: true });

        // SSE format: lines separated by \n\n, each data line starts with "data: "
        const parts = buffer.split('\n\n');
        // Keep the last (possibly incomplete) chunk in the buffer
        buffer = parts.pop() ?? '';

        for (const part of parts) {
          this.processSSEChunk(part);
        }
      }
    } catch (err) {
      if (!this.closed) {
        throw err;
      }
    } finally {
      reader.releaseLock();
    }

    // Stream ended normally — server closed the connection
    if (!this.closed) {
      this.options.onStateChange('disconnected');
    }
  }

  private processSSEChunk(chunk: string): void {
    for (const line of chunk.split('\n')) {
      const trimmed = line.trim();

      if (trimmed === '' || trimmed.startsWith(':')) {
        // Empty line or comment — skip
        continue;
      }

      if (trimmed.startsWith('data: ')) {
        const json = trimmed.slice(6);
        this.options.onRawLine?.(json);

        try {
          const event = JSON.parse(json) as StreamEvent;
          this.options.onEvent(event);
        } catch {
          // Non-JSON data line — ignore
        }
      }
    }
  }

  private async maybeRetry(): Promise<void> {
    if (this.closed) return;

    if (this.retryCount >= this.options.maxRetries) {
      this.options.onEvent({
        type: 'error',
        message: `Gave up after ${this.options.maxRetries} retries`,
        retryable: false,
      });
      return;
    }

    this.retryCount++;
    const delay = this.options.retryDelayMs * Math.pow(1.5, this.retryCount - 1);
    await new Promise((resolve) => setTimeout(resolve, delay));

    if (!this.closed) {
      await this.connect();
    }
  }
}
