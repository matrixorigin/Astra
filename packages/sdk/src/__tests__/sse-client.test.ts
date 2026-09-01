import { SSEClient, parseSseDataEvents } from "../sse-client";
import type {
  AgentWaitingEvent,
  ConnectionState,
  RunBlockedEvent,
  RunStartedEvent,
  RunWaitingEvent,
  StreamEvent,
  ToolCallEvent,
} from "../types";
import { readSseFixture } from "./sse-fixture-helpers";

// ─── Mock Fetch + ReadableStream ────────────────────────────────────

function createMockStream(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let idx = 0;
  return new ReadableStream({
    pull(controller) {
      if (idx < chunks.length) {
        controller.enqueue(encoder.encode(chunks[idx]));
        idx++;
      } else {
        controller.close();
      }
    },
  });
}

function createErroredStream(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let idx = 0;
  return new ReadableStream({
    pull(controller) {
      if (idx < chunks.length) {
        controller.enqueue(encoder.encode(chunks[idx]));
        idx++;
      } else {
        controller.error(new Error("socket reset after terminal"));
      }
    },
  });
}

function mockFetchStream(chunks: string[], status = 200) {
  return vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? "OK" : "Error",
    body: createMockStream(chunks),
    headers: new Headers(),
  } as unknown as Response);
}

function mockFetchNoBody(status = 200) {
  return vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    statusText: "OK",
    body: null,
    headers: new Headers(),
  } as unknown as Response);
}

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

// ─── Basic Connection ──────────────────────────────────────────────

describe("SSEClient — Connection", () => {
  test("connect sends Accept header and token", async () => {
    const chunks = ['data: {"type":"text_delta","content":"hi"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      token: "my-token",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost/stream");
    expect(call[1].headers["Accept"]).toBe("text/event-stream");
    expect(call[1].headers["Authorization"]).toBe("Bearer my-token");
    expect(call[1].headers["Cache-Control"]).toBe("no-cache");
  });

  test("connect without token omits Authorization", async () => {
    const chunks = ['data: {"type":"text_delta","content":"hi"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[1].headers["Authorization"]).toBeUndefined();
  });

  test("custom headers are sent", async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const client = new SSEClient({
      url: "http://localhost/stream",
      headers: { "X-Custom": "value" },
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[1].headers["X-Custom"]).toBe("value");
  });

  test("POST method sends body and Content-Type", async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const body = JSON.stringify({ message: "hello" });
    const client = new SSEClient({
      url: "http://localhost/stream",
      method: "POST",
      body,
      onEvent: () => {},
    });
    await client.connect();

    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[1].method).toBe("POST");
    expect(call[1].body).toBe(body);
    expect(call[1].headers["Content-Type"]).toBe("application/json");
  });

  test("required terminal reports a partial EOF as a transport error", async () => {
    globalThis.fetch = mockFetchStream([
      'data: {"type":"text_delta","content":"partial"}\n\n',
    ]);
    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (event) => events.push(event),
      maxRetries: 0,
      requireTerminalEvent: true,
    });

    await client.connect();

    expect(events.at(-1)).toMatchObject({
      type: "error",
      retryable: false,
    });
    expect((events.at(-1) as { message?: string }).message).toContain(
      "terminal event",
    );
  });

  test("terminal event wins over a trailing socket reset", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      statusText: "OK",
      body: createErroredStream([
        'data: {"type":"turn_complete","assistant_text":"done"}\n\n',
      ]),
      headers: new Headers(),
    } as unknown as Response);
    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (event) => events.push(event),
      maxRetries: 0,
      requireTerminalEvent: true,
    });

    await client.connect();

    expect(events).toEqual([
      { type: "turn_complete", assistant_text: "done" },
    ]);
  });
});

// ─── Event Parsing ─────────────────────────────────────────────────

describe("SSEClient — Event Parsing", () => {
  test("parses single SSE event", async () => {
    const chunks = ['data: {"type":"text_delta","content":"hello"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe("text_delta");
  });

  test("parses multiple events in one chunk", async () => {
    const chunks = [
      'data: {"type":"run_started","run_id":"r1","workspace":{"kind":"edge_workspace","cwd":"/repo"},"executor":{"kind":"edge_agent","executor_id":"edge-1"},"transport":"edge_ws","fallback_policy":"disabled"}\n\n' +
        'data: {"type":"text_delta","content":"a"}\n\n' +
        'data: {"type":"text_delta","content":"b"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(3);
    const started = events[0] as RunStartedEvent;
    expect(started.type).toBe("run_started");
    expect(started.workspace?.kind).toBe("edge_workspace");
    expect(started.executor?.executor_id).toBe("edge-1");
    expect(started.transport).toBe("edge_ws");
    expect(started.fallback_policy).toBe("disabled");
    expect(events[1].type).toBe("text_delta");
    expect(events[2].type).toBe("text_delta");
  });

  test("parses events split across chunks", async () => {
    // Event split in the middle
    const chunks = ['data: {"type":"text_del', 'ta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe("text_delta");
  });

  test("ignores malformed JSON lines", async () => {
    const chunks = [
      "data: not-json\n\n" + 'data: {"type":"text_delta","content":"ok"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(1);
    expect(events[0].type).toBe("text_delta");
  });

  test("handles data: with and without space prefix", async () => {
    const chunks = [
      'data:{"type":"text_delta","content":"no-space"}\n\n' +
        'data: {"type":"text_delta","content":"with-space"}\n\n',
    ];
    globalThis.fetch = mockFetchStream(chunks);

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
    });
    await client.connect();

    expect(events).toHaveLength(2);
  });

  test("calls onRawLine for each line", async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const lines: string[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      onRawLine: (l) => lines.push(l),
    });
    await client.connect();

    expect(lines.length).toBeGreaterThanOrEqual(1);
    expect(lines[0]).toContain("data:");
  });
});

// ─── State Changes ─────────────────────────────────────────────────

describe("SSEClient — State Changes", () => {
  test("fires connecting → connected → disconnected", async () => {
    const chunks = ['data: {"type":"text_delta","content":"x"}\n\n'];
    globalThis.fetch = mockFetchStream(chunks);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
    });
    await client.connect();

    expect(states[0]).toBe("connecting");
    expect(states[1]).toBe("connected");
    expect(states[2]).toBe("disconnected");
  });

  test("fires error state on non-OK response", async () => {
    globalThis.fetch = mockFetchStream([], 500);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
      maxRetries: 0,
    });
    await client.connect();

    expect(states).toContain("error");
  });

  test("fires error state when body is null", async () => {
    globalThis.fetch = mockFetchNoBody(200);

    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
      maxRetries: 0,
    });
    await client.connect();

    expect(states).toContain("error");
  });
});

// ─── Close / Abort ─────────────────────────────────────────────────

describe("SSEClient — Close", () => {
  test("close() fires disconnected state", () => {
    const states: ConnectionState[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      onStateChange: (s) => states.push(s),
    });
    client.close();
    expect(states).toContain("disconnected");
  });

  test("close() before connect is safe", () => {
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
    });
    // Should not throw
    client.close();
  });

  test("close() during connect aborts fetch", async () => {
    let fetchAborted = false;
    globalThis.fetch = vi
      .fn()
      .mockImplementation((_url: string, opts: { signal: AbortSignal }) => {
        opts.signal.addEventListener("abort", () => {
          fetchAborted = true;
        });
        // Return a promise that never resolves until aborted
        return new Promise((_resolve, reject) => {
          opts.signal.addEventListener("abort", () => {
            reject(new DOMException("aborted", "AbortError"));
          });
        });
      });

    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: () => {},
      maxRetries: 0,
    });

    const connectPromise = client.connect();
    // Give connect time to call fetch
    await new Promise((r) => setTimeout(r, 10));
    client.close();
    await connectPromise;

    expect(fetchAborted).toBe(true);
  });
});

// ─── AbortSignal ───────────────────────────────────────────────────

describe("SSEClient — AbortSignal", () => {
  test("respects external AbortSignal", async () => {
    const controller = new AbortController();
    // Abort immediately
    controller.abort();

    globalThis.fetch = vi
      .fn()
      .mockRejectedValue(new DOMException("aborted", "AbortError"));

    const events: StreamEvent[] = [];
    const client = new SSEClient({
      url: "http://localhost/stream",
      onEvent: (e) => events.push(e),
      signal: controller.signal,
    });

    await client.connect();
    expect(events).toHaveLength(0);
  });
});

// ─── parseSseDataEvents ─────────────────────────────────────────────

describe("parseSseDataEvents", () => {
  test("parses multiple SSE blocks", () => {
    const raw =
      'data: {"type":"session_info","session_id":"s1","run_id":"r1"}\n\n' +
      'data: {"type":"text_delta","content":"hi"}\n\n';
    const events = parseSseDataEvents(raw);
    expect(events).toHaveLength(2);
    expect(events[0].type).toBe("session_info");
    expect(events[1]).toMatchObject({ type: "text_delta", content: "hi" });
  });

  test("parses __fixtures__/sse/workspace-stream.txt", () => {
    const events = parseSseDataEvents(readSseFixture("workspace-stream.txt"));
    expect(events.map((e) => e.type)).toEqual([
      "session_info",
      "text_delta",
      "text_delta",
      "usage",
      "turn_complete",
    ]);
  });

  test("parses execution-boundary waiting and blocked events", () => {
    const raw =
      'data: {"type":"run_waiting","run_id":"r1","reason":"executor offline","waiting_for":"edge","workspace":{"kind":"edge_workspace"},"executor":{"kind":"edge_agent","status":"offline"},"transport":"edge_ws","fallback_policy":"disabled"}\n\n' +
      'data: {"type":"run_blocked","call_id":"c1","tool":"shell","reason":"transport_disconnected","message":"edge disconnected","executor":{"kind":"edge_agent"},"transport":"edge_ws"}\n\n' +
      'data: {"type":"run_blocked","call_id":"c2","tool":"bash","reason":"workspace_executor_unavailable","message":"workspace is not routed","workspace":{"kind":"git_checkout"},"executor":{"kind":"orchestrator_managed","status":"degraded"},"transport":"sandbox_resident_agent"}\n\n' +
      'data: {"type":"agent_waiting","agent_id":"a1","run_id":"r2","status":"waiting","reason":"child executor offline"}\n\n';

    const events = parseSseDataEvents(raw);
    expect(events).toHaveLength(4);

    const runWaiting: RunWaitingEvent = events[0] as RunWaitingEvent;
    expect(runWaiting.type).toBe("run_waiting");
    expect(runWaiting.executor?.status).toBe("offline");
    expect(runWaiting.fallback_policy).toBe("disabled");

    const blocked: RunBlockedEvent = events[1] as RunBlockedEvent;
    expect(blocked.type).toBe("run_blocked");
    expect(blocked.tool).toBe("shell");
    expect(blocked.transport).toBe("edge_ws");

    const unsupported: RunBlockedEvent = events[2] as RunBlockedEvent;
    expect(unsupported.type).toBe("run_blocked");
    expect(unsupported.reason).toBe("workspace_executor_unavailable");
    expect(unsupported.workspace?.kind).toBe("git_checkout");
    expect(unsupported.executor?.status).toBe("degraded");

    const agentWaiting: AgentWaitingEvent = events[3] as AgentWaitingEvent;
    expect(agentWaiting.type).toBe("agent_waiting");
    expect(agentWaiting.agent_id).toBe("a1");
  });

  test("parses execution bindings on tool_call events", () => {
    const raw =
      'data: {"type":"tool_call","tool_call":{"id":"call-1","function":{"name":"bash","arguments":"{}"}},"workspace":{"kind":"edge_workspace","cwd":"/repo"},"executor":{"kind":"edge_agent","executor_id":"edge-1","status":"online"},"transport":"edge_ws","fallback_policy":"disabled"}\n\n';

    const events = parseSseDataEvents(raw);
    expect(events).toHaveLength(1);

    const toolCall = events[0] as ToolCallEvent;
    expect(toolCall.type).toBe("tool_call");
    expect(toolCall.tool_call.id).toBe("call-1");
    expect(toolCall.workspace?.kind).toBe("edge_workspace");
    expect(toolCall.workspace?.cwd).toBe("/repo");
    expect(toolCall.executor?.executor_id).toBe("edge-1");
    expect(toolCall.transport).toBe("edge_ws");
    expect(toolCall.fallback_policy).toBe("disabled");
  });
});
