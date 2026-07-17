import { AstraClient, AstraApiError, chatRequestToWire } from "../client";
import { PATH_SESSIONS } from "../paths";

// ─── Helpers ────────────────────────────────────────────────────────

function mockFetch(
  status: number,
  body: unknown = {},
  headers?: Record<string, string>,
) {
  return vi.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(body),
    text: () =>
      Promise.resolve(typeof body === "string" ? body : JSON.stringify(body)),
    headers: new Headers(headers),
  } as unknown as Response);
}

let originalFetch: typeof globalThis.fetch;

beforeEach(() => {
  originalFetch = globalThis.fetch;
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

function createClient(token = "test-access-token") {
  return new AstraClient({
    baseUrl: "http://localhost:8000",
    accessToken: token,
  });
}

// ─── Auth ─────────────────────────────────────────────────────────

describe("AstraClient — Auth", () => {
  test("login stores tokens and returns result", async () => {
    const authResult = {
      access_token: "new-at",
      refresh_token: "new-rt",
      token_type: "Bearer",
      expires_in: 3600,
    };
    globalThis.fetch = mockFetch(200, authResult);

    const client = createClient();
    const result = await client.login("alice", "pass");

    expect(result).toEqual(authResult);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/auth/login");
    expect(JSON.parse(call[1].body)).toEqual({
      username: "alice",
      password: "pass",
    });
  });

  test("register stores tokens", async () => {
    const authResult = {
      access_token: "reg-at",
      refresh_token: "reg-rt",
      token_type: "Bearer",
      expires_in: 3600,
      user_id: "u2",
      username: "bob",
      email: "bob@users.local.astra",
    };
    globalThis.fetch = mockFetch(200, authResult);

    const client = createClient();
    const result = await client.register("bob", "pass");

    expect(result).toEqual(authResult);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/auth/register");
    const regBody = JSON.parse(call[1].body);
    expect(regBody.username).toBe("bob");
    expect(regBody.password).toBe("pass");
    expect(regBody.email).toBe("bob@users.local.astra");
  });

  test("logout clears tokens", async () => {
    globalThis.fetch = mockFetch(200);

    const client = createClient("tok");
    client.setTokens("tok", "rt");
    await client.logout();

    // Next call should have no Authorization header
    globalThis.fetch = mockFetch(200, {
      user_id: "u1",
      username: "a",
      email: "a@b.c",
      display_name: null,
    });
    await client.getMe().catch(() => {});
    const headers = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][1].headers;
    expect(headers["Authorization"]).toBeUndefined();
  });

  test("getMe returns user info", async () => {
    const user = {
      user_id: "u1",
      username: "alice",
      email: "alice@example.com",
      display_name: null,
    };
    globalThis.fetch = mockFetch(200, user);

    const client = createClient();
    const result = await client.getMe();
    expect(result).toEqual(user);
  });
});

// ─── Sessions ─────────────────────────────────────────────────────

describe("AstraClient — Sessions", () => {
  test("createSession", async () => {
    const raw = {
      session_id: "s1",
      user_id: "u1",
      agent_id: null,
      title: "T",
      status: "active",
      event_count: 0,
      created_at: "2025-01-01T00:00:00",
      updated_at: "2025-01-01T00:00:00",
      ended_at: null,
      metadata: {},
    };
    globalThis.fetch = mockFetch(200, raw);

    const result = await createClient().createSession();
    expect(result.sessionId).toBe("s1");
    expect(result.createdAt).toBe("2025-01-01T00:00:00");
  });

  test("createRuntimeSession returns raw runtime payload", async () => {
    const raw = {
      session_id: "s1",
      user_id: "u1",
      agent_id: null,
      title: "T",
      status: "active",
      event_count: 0,
      created_at: "2025-01-01T00:00:00",
      updated_at: "2025-01-01T00:00:00",
      ended_at: null,
      metadata: { source: "web_v1" },
    };
    globalThis.fetch = mockFetch(200, raw);

    const result = await createClient().createRuntimeSession({
      agent_id: null,
      title: "T",
      metadata: { source: "web_v1" },
    });
    expect(result.metadata?.source).toBe("web_v1");
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.metadata.source).toBe("web_v1");
  });

  test("getSession", async () => {
    const raw = {
      session_id: "s2",
      user_id: "u1",
      agent_id: null,
      title: null,
      status: "active",
      event_count: 1,
      created_at: "2025-01-01T00:00:00",
      updated_at: null,
      ended_at: null,
      metadata: {},
    };
    globalThis.fetch = mockFetch(200, raw);

    const result = await createClient().getSession("s2");
    expect(result.sessionId).toBe("s2");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/sessions/s2");
  });

  test("getRuntimeSession returns raw metadata", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "s2",
      user_id: "u1",
      agent_id: null,
      title: null,
      status: "active",
      event_count: 1,
      created_at: "2025-01-01T00:00:00",
      updated_at: null,
      ended_at: null,
      metadata: { current_model: "m1" },
    });

    const result = await createClient().getRuntimeSession("s2");
    expect(result.metadata?.current_model).toBe("m1");
  });

  test("listSessions", async () => {
    globalThis.fetch = mockFetch(200, {
      sessions: [],
      total: 0,
      limit: 50,
      next_cursor: null,
    });
    const result = await createClient().listSessions();
    expect(Array.isArray(result)).toBe(true);
  });

  test("listRuntimeSessions preserves pagination envelope", async () => {
    globalThis.fetch = mockFetch(200, {
      sessions: [],
      total: null,
      limit: 2,
      next_cursor: {
        updated_at: "2026-06-29T08:00:00Z",
        session_id: "session-2",
      },
    });
    const result = await createClient().listRuntimeSessions({
      limit: 2,
      cursor: {
        updated_at: "2026-06-29T08:00:00Z",
        session_id: "session-1",
      },
    });
    expect(result.total).toBeNull();
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("limit=2");
    expect(url).toContain("after_updated_at=2026-06-29T08%3A00%3A00Z");
    expect(url).toContain("after_session_id=session-1");
  });

  test("deleteSession", async () => {
    globalThis.fetch = mockFetch(204);
    await createClient().deleteSession("s3");
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/sessions/s3");
    expect(call[1].method).toBe("DELETE");
  });

  test("getSessionAudit", async () => {
    const summary = {
      session_id: "s4",
      status: "closed",
      turn_count: 2,
      tokens_in: 10,
      tokens_out: 20,
      tool_calls_total: 3,
      tool_calls_failed: 0,
      error_count: 0,
      stall_count: 0,
      checkpoint_count: 0,
      compact_count: 0,
      execution_boundary_opened_count: 0,
      execution_boundary_committed_count: 0,
      execution_boundary_aborted_count: 0,
      approval_required_count: 0,
      approval_decision_count: 0,
      approval_timeout_count: 0,
      models_used: [],
      duration_secs: 1.5,
      created_at: "2025-01-01",
      ended_at: null,
    };
    globalThis.fetch = mockFetch(200, summary);
    const result = await createClient().getSessionAudit("s4");
    expect(result.session_id).toBe("s4");
    expect(result.turn_count).toBe(2);
  });
});

// ─── Runs ─────────────────────────────────────────────────────────

describe("AstraClient — Runs", () => {
  test("createRun", async () => {
    const chatResp = { session_id: "s1", run_id: "r1", status: "running" };
    globalThis.fetch = mockFetch(200, chatResp);

    const result = await createClient().createRun({
      message: "hello",
      selectedModel: { model: "test-model" },
    });
    expect(result.runId).toBe("r1");
    expect(result.sessionId).toBe("s1");
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/chat");
    expect(call[1].method).toBe("POST");
  });

  test("getRunStatus", async () => {
    const raw = {
      run_id: "r1",
      session_id: "s1",
      status: "completed",
      waiting_for: null,
      events_count: 5,
      workspace: {
        kind: "edge_workspace",
        display_name: "MacBook Pro",
        cwd: "/Users/test/project",
        authority: "read_write",
        fallback_policy: "disabled",
      },
      executor: {
        kind: "edge_agent",
        executor_id: "edge-1",
        display_name: "MacBook Pro",
        transport: "edge_ws",
        status: "online",
      },
      transport: "edge_ws",
      fallback_policy: "disabled",
    };
    globalThis.fetch = mockFetch(200, raw);

    const result = await createClient().getRunStatus("r1");
    expect(result.status).toBe("completed");
    expect(result.eventsCount).toBe(5);
    expect(result.workspace).toMatchObject({
      kind: "edge_workspace",
      cwd: "/Users/test/project",
    });
    expect(result.executor).toMatchObject({
      kind: "edge_agent",
      executor_id: "edge-1",
      transport: "edge_ws",
    });
    expect(result.transport).toBe("edge_ws");
    expect(result.fallbackPolicy).toBe("disabled");
  });

  test("getRunStatus normalizes blocked execution-boundary state", async () => {
    globalThis.fetch = mockFetch(200, {
      run_id: "r-blocked",
      session_id: "s1",
      status: "blocked",
      waiting_for: "workspace_executor_unavailable",
      events_count: 7,
      workspace: {
        kind: "git_checkout",
        display_name: "Cloud checkout",
        cwd: "/checkout/repo",
        authority: "read_only",
        fallback_policy: "disabled",
      },
      executor: {
        kind: "orchestrator_managed",
        executor_id: "orchestrator-managed",
        display_name: "Orchestrator-managed executor",
        transport: "sandbox_resident_agent",
        status: "degraded",
      },
      transport: "sandbox_resident_agent",
      fallback_policy: "disabled",
    });

    const result = await createClient().getRunStatus("r-blocked");
    expect(result.status).toBe("blocked");
    expect(result.waitingFor).toBe("workspace_executor_unavailable");
    expect(result.workspace?.kind).toBe("git_checkout");
    expect(result.executor?.status).toBe("degraded");
    expect(result.transport).toBe("sandbox_resident_agent");
    expect(result.fallbackPolicy).toBe("disabled");
  });

  test("cancelRun", async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().cancelRun("r1");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/chat/runs/r1");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].method,
    ).toBe("DELETE");
  });

  test("pauseRun", async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().pauseRun("r1");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/chat/runs/r1/pause");
  });

  test("resumeRun", async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().resumeRun("r1");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/chat/runs/r1/resume");
  });

  test("getRunEvents with startIndex", async () => {
    const sse =
      'data: {"type":"text_delta","content":"x"}\n\n' +
      'data: {"type":"turn_complete"}\n\n';
    globalThis.fetch = mockFetch(200, sse);

    const events = await createClient().getRunEvents("r1", 5);
    expect(events).toHaveLength(2);
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("last_index=5");
  });

  test("getRunProjection uses bounded projection endpoint", async () => {
    globalThis.fetch = mockFetch(200, {
      run_id: "r1",
      session_id: "s1",
      status: "running",
      workspace: {
        kind: "edge_workspace",
        display_name: "MacBook Pro",
        cwd: "/Users/xupeng/github/astra",
        authority: "read_write",
        fallback_policy: "disabled",
      },
      executor: {
        kind: "edge_agent",
        executor_id: "edge-1",
        display_name: "MacBook Pro",
        transport: "edge_ws",
        status: "online",
      },
      transport: "edge_ws",
      fallback_policy: "disabled",
      run_event_high_watermark: 7,
      projection_event_idx: 6,
      projection_updated_at: "2026-06-10T00:00:00.000Z",
      projection_hash: "hash",
      total_prompt_tokens: 1,
      total_completion_tokens: 2,
      total_tool_calls: 3,
      observability: {
        has_durable_projection: true,
        observability_available: true,
        projection_lag_events: 1,
        prompt_request_count: 0,
      },
      recent_events: [
        { type: "tool_call_start", call_id: "call-1", tool: "bash" },
      ],
    });

    const projection = await createClient().getRunProjection("r1", {
      recentLimit: 25,
    });
    expect(projection.recent_events).toHaveLength(1);
    expect(projection.workspace?.kind).toBe("edge_workspace");
    expect(projection.workspace?.cwd).toBe("/Users/xupeng/github/astra");
    expect(projection.executor?.executor_id).toBe("edge-1");
    expect(projection.transport).toBe("edge_ws");
    expect(projection.fallback_policy).toBe("disabled");
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/chat/runs/r1/projection");
    expect(url).toContain("recent_limit=25");
  });

  test("repairRunProjection posts to repair endpoint", async () => {
    globalThis.fetch = mockFetch(200, {
      repaired: true,
      projection: {
        run_id: "r1",
        session_id: "s1",
        status: "running",
        run_event_high_watermark: 7,
        projection_event_idx: 7,
        projection_updated_at: "2026-06-10T00:00:00.000Z",
        projection_hash: "hash",
        total_prompt_tokens: 1,
        total_completion_tokens: 2,
        total_tool_calls: 3,
        observability: {
          has_durable_projection: true,
          observability_available: true,
          projection_lag_events: 0,
          prompt_request_count: 0,
        },
        recent_events: [],
      },
    });

    const repaired = await createClient().repairRunProjection("r1", {
      recentLimit: 10,
    });

    expect(repaired.repaired).toBe(true);
    expect(repaired.projection.projection_event_idx).toBe(7);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/chat/runs/r1/projection/repair");
    expect(call[0]).toContain("recent_limit=10");
    expect(call[1].method).toBe("POST");
  });

  test("listRuns normalizes snake_case runs", async () => {
    globalThis.fetch = mockFetch(200, {
      runs: [
        {
          run_id: "r1",
          session_id: "s1",
          parent_run_id: "root-1",
          root_run_id: "root-1",
          depth: 1,
          status: "running",
          waiting_for: null,
          events_count: 3,
          workspace: {
            kind: "server_sandbox",
            display_name: "Server sandbox",
            cwd: "/tmp/astra-workspaces/s1",
            authority: "read_write",
            fallback_policy: "disabled",
          },
          executor: {
            kind: "server_local",
            executor_id: "server-local",
            display_name: "Server sandbox",
            transport: "server_local",
            status: "online",
          },
          transport: "server_local",
          fallback_policy: "disabled",
        },
      ],
      total: null,
      limit: 50,
      next_cursor: null,
    });
    const r = await createClient().listRuns();
    expect(r.runs[0].runId).toBe("r1");
    expect(r.runs[0].parentRunId).toBe("root-1");
    expect(r.runs[0].rootRunId).toBe("root-1");
    expect(r.runs[0].depth).toBe(1);
    expect(r.runs[0].eventsCount).toBe(3);
    expect(r.runs[0].workspace?.kind).toBe("server_sandbox");
    expect(r.runs[0].executor?.kind).toBe("server_local");
    expect(r.runs[0].transport).toBe("server_local");
    expect(r.runs[0].fallbackPolicy).toBe("disabled");
    expect(r.total).toBeNull();
    expect(r.nextCursor).toBeNull();
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/runs");
  });

  test("listRuns sends seek cursor and preserves nullable total", async () => {
    globalThis.fetch = mockFetch(200, {
      runs: [],
      total: null,
      limit: 2,
      next_cursor: {
        updated_at: "2026-06-29T08:00:00.000000",
        run_id: "run-2",
      },
    });

    const r = await createClient().listRuns({
      limit: 2,
      cursor: {
        updatedAt: "2026-06-29T08:30:00.000000",
        runId: "run-1",
      },
    });

    expect(r.total).toBeNull();
    expect(r.nextCursor).toEqual({
      updatedAt: "2026-06-29T08:00:00.000000",
      runId: "run-2",
    });
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("/runs");
    expect(url).toContain("limit=2");
    expect(url).toContain("after_updated_at=2026-06-29T08%3A30%3A00.000000");
    expect(url).toContain("after_run_id=run-1");
    expect(url).not.toContain("offset=");
  });

  test("delegateRun posts delegation body", async () => {
    globalThis.fetch = mockFetch(200, {
      delegation_id: "d1",
      status: "completed",
      agent_results: [],
      aggregated_output: null,
      total_prompt_tokens: 1,
      total_completion_tokens: 2,
      total_tool_calls: 0,
    });
    const body = {
      delegation_id: "d1",
      parent_run_id: "r0",
      task: "do work",
      pattern: {
        sequential: {
          agent_ids: ["a1"],
          stop_on_success: true,
          timeout_sec: 0,
        },
      },
      user_id: "u1",
      depth: 0,
    };
    const res = await createClient().delegateRun("r0", body);
    expect(res.delegation_id).toBe("d1");
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/chat/runs/r0/delegate");
    expect(JSON.parse(call[1].body).task).toBe("do work");
  });

  test("listDelegations", async () => {
    globalThis.fetch = mockFetch(200, {
      parent_run_id: "r0",
      sub_run_ids: ["sr1"],
    });
    const r = await createClient().listDelegations("r0");
    expect(r.sub_run_ids).toEqual(["sr1"]);
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/delegations");
  });

  test("pauseDelegations", async () => {
    globalThis.fetch = mockFetch(200, { parent_run_id: "r0", affected: 2 });
    const r = await createClient().pauseDelegations("r0");
    expect(r.affected).toBe(2);
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/delegations/pause");
  });

  test("resumeDelegations", async () => {
    globalThis.fetch = mockFetch(200, { parent_run_id: "r0", affected: 1 });
    await createClient().resumeDelegations("r0");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/delegations/resume");
  });
});

// ─── Session lifecycle & reflect ───────────────────────────────────

describe("AstraClient — Session lifecycle and reflect", () => {
  const sessWire = {
    session_id: "sx",
    user_id: "u",
    agent_id: null,
    title: null,
    status: "active",
    event_count: 0,
    created_at: "2025-01-01T00:00:00",
    updated_at: null,
    ended_at: null,
    metadata: {},
  };

  test("updateSession uses PUT", async () => {
    globalThis.fetch = mockFetch(200, sessWire);
    await createClient().updateSession("sx", { title: "New" });
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[1].method).toBe("PUT");
    expect(JSON.parse(call[1].body).title).toBe("New");
    expect(call[0]).toContain("/sessions/sx");
  });

  test("updateRuntimeSession returns raw runtime payload", async () => {
    globalThis.fetch = mockFetch(200, {
      ...sessWire,
      status: "archived",
      metadata: { current_model: "m2" },
    });
    const result = await createClient().updateRuntimeSession("sx", {
      status: "archived",
      metadata: { current_model: "m2" },
    });
    expect(result.status).toBe("archived");
    expect(result.metadata?.current_model).toBe("m2");
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.status).toBe("archived");
  });

  test("getSessionTranscript builds transcript query", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "sx",
      items: [],
      next_before_seq: null,
      has_more: false,
    });
    const result = await createClient().getSessionTranscript("sx", {
      before_seq: 10,
      limit: 5,
    });
    expect(result.items).toEqual([]);
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("/sessions/sx/transcript");
    expect(url).toContain("before_seq=10");
    expect(url).toContain("limit=5");
  });

  test("listSessionArtifacts builds artifacts query", async () => {
    globalThis.fetch = mockFetch(200, { artifacts: [] });
    const result = await createClient().listSessionArtifacts("sx", {
      limit: 50,
      offset: 5,
    });
    expect(result.artifacts).toEqual([]);
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("/sessions/sx/artifacts");
    expect(url).toContain("limit=50");
    expect(url).toContain("offset=5");
  });

  test("closeSession", async () => {
    globalThis.fetch = mockFetch(200, sessWire);
    await createClient().closeSession("sx");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/sessions/sx/close");
  });

  test("resumeSession", async () => {
    globalThis.fetch = mockFetch(200, sessWire);
    await createClient().resumeSession("sx");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/sessions/sx/resume");
  });

  test("cancelSession", async () => {
    globalThis.fetch = mockFetch(200, sessWire);
    await createClient().cancelSession("sx");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/sessions/sx/cancel");
  });

  test("getSessionActivity", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "sx",
      activities: [],
      total: 0,
    });
    const r = await createClient().getSessionActivity("sx", { limit: 5 });
    expect(r.total).toBe(0);
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/activity");
    expect(url).toContain("limit=5");
  });

  test("getSessionReflect", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "sx",
      focus: "auto",
      overview: {},
      diagnoses: [],
      insights: [],
      recommendations: [],
    });
    await createClient().getSessionReflect("sx", { focus: "tools", last_n: 5 });
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/chat/session/sx/reflect");
    expect(url).toContain("focus=tools");
    expect(url).toContain("last_n=5");
  });

  test("getSessionReflect normalizes omitted collection fields", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "sx",
      focus: "auto",
      overview: null,
      recommendations: [null, "", "Continue with verification."],
    });
    const report = await createClient().getSessionReflect("sx");
    expect(report).toMatchObject({
      session_id: "sx",
      focus: "auto",
      overview: {},
      diagnoses: [],
      insights: [],
      recommendations: ["Continue with verification."],
    });
  });

  test("getSessionDecisionTrace", async () => {
    globalThis.fetch = mockFetch(200, {
      session_id: "sx",
      focus: "tool_selection",
      overview: {},
      diagnoses: [],
      insights: [],
      recommendations: [],
    });
    await createClient().getSessionDecisionTrace("sx");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("decision-trace");
  });
});

// ─── Events & edges ────────────────────────────────────────────────

describe("AstraClient — Events and edges", () => {
  test("getSessionEvents", async () => {
    globalThis.fetch = mockFetch(200, {
      events: [],
      total: 0,
      limit: 100,
      next_cursor: null,
    });
    await createClient().getSessionEvents("sid", {
      cursor: {
        created_at: "2026-06-29T08:00:00Z",
        event_id: "event-1",
      },
    });
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/events/session/sid");
    expect(url).toContain("after_created_at=2026-06-29T08%3A00%3A00Z");
    expect(url).toContain("after_event_id=event-1");
  });

  test("listEvents", async () => {
    globalThis.fetch = mockFetch(200, {
      events: [],
      total: null,
      limit: 50,
      next_cursor: null,
    });
    const result = await createClient().listEvents({ eventType: "tool_call", limit: 20 });
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0];
    expect(url).toContain("/events?");
    expect(url).toContain("event_type=tool_call");
    expect(url).toContain("limit=20");
    expect(result.total).toBeNull();
  });

  test("getCausalChain", async () => {
    globalThis.fetch = mockFetch(200, []);
    const r = await createClient().getCausalChain("cc1");
    expect(r).toEqual([]);
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/events/causal-chain/cc1");
  });

  test("getEdgesStatus", async () => {
    globalThis.fetch = mockFetch(200, { edges: [] });
    const r = await createClient().getEdgesStatus();
    expect(r.edges).toEqual([]);
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/edges/status");
  });
});

// ─── Memory ─────────────────────────────────────────────────────────

describe("AstraClient — Memory", () => {
  test("memoryStore", async () => {
    globalThis.fetch = mockFetch(200, { id: "m1" });
    const result = await createClient().memoryStore({ content: "hello" });
    expect(result.id).toBe("m1");
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.content).toBe("hello");
  });

  test("memorySearch", async () => {
    globalThis.fetch = mockFetch(200, [
      { id: "m1", content: "hello", score: 0.9 },
    ]);
    const result = await createClient().memorySearch("hello");
    expect(result).toHaveLength(1);
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.query).toBe("hello");
    expect(body.top_k).toBe(10);
  });

  test("memoryRetrieve", async () => {
    globalThis.fetch = mockFetch(200, []);
    await createClient().memoryRetrieve("query", 3);
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.top_k).toBe(3);
  });

  test("memoryPurge", async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().memoryPurge("old-topic");
    const body = JSON.parse(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][1].body,
    );
    expect(body.topic).toBe("old-topic");
  });
});

// ─── Skills ─────────────────────────────────────────────────────────

describe("AstraClient — Models", () => {
  test("listModels accepts array payloads", async () => {
    globalThis.fetch = mockFetch(200, [
      { name: "m1", provider: "p1", is_active: true },
      null,
    ]);

    const result = await createClient().listModels();
    expect(result).toHaveLength(1);
    expect(result[0].name).toBe("m1");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/models");
  });

  test("listModels accepts envelope payloads", async () => {
    globalThis.fetch = mockFetch(200, {
      models: [{ model_id: "m2", provider: "p2" }],
    });

    const result = await createClient().listModels();
    expect(result[0].model_id).toBe("m2");
  });
});

describe("AstraClient — Skills", () => {
  test("listSkills", async () => {
    globalThis.fetch = mockFetch(200, {
      skills: [
        {
          skill_id: "id1",
          skill_name: "bash",
          version: "1",
          description: "d",
          status: "active",
        },
      ],
      total: 1,
      limit: 50,
      offset: 0,
    });
    const result = await createClient().listSkills();
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("id1");
    expect(result[0].name).toBe("bash");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toContain("/skills");
  });

  test("listRuntimeSkills preserves pagination envelope", async () => {
    globalThis.fetch = mockFetch(200, {
      skills: [
        {
          skill_id: "id2",
          skill_name: "web-search",
          version: "2",
          description: "Search the web",
          source: "database",
          category: "research",
          status: "published",
        },
      ],
      total: 11,
      limit: 1,
      next_cursor: {
        skill_name: "web-search",
        version: "2",
        skill_id: "id2",
      },
    });

    const result = await createClient().listRuntimeSkills({
      limit: 1,
      cursor: {
        skill_name: "code-search",
        version: "1",
        skill_id: "id1",
      },
    });
    expect(result.total).toBe(11);
    expect(result.skills?.[0]?.source).toBe("database");
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("/skills");
    expect(url).toContain("limit=1");
    expect(url).toContain("after_skill_name=code-search");
    expect(url).toContain("after_version=1");
    expect(url).toContain("after_skill_id=id1");
  });
});

// ─── pathPrefix ────────────────────────────────────────────────────

describe("AstraClient — pathPrefix", () => {
  test("prepends pathPrefix to auth login", async () => {
    globalThis.fetch = mockFetch(200, {
      access_token: "a",
      refresh_token: "r",
      token_type: "Bearer",
      expires_in: 1,
    });
    const client = new AstraClient({
      baseUrl: "http://localhost:8000",
      pathPrefix: "/api",
    });
    await client.login("u", "p");
    expect(
      (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0],
    ).toBe("http://localhost:8000/api/auth/login");
  });
});

// ─── §5.5 thin protocol ────────────────────────────────────────────

describe("AstraClient — thin protocol", () => {
  test("postToolResult sends JSON and edge header", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () => Promise.resolve("{}"),
      headers: new Headers(),
    } as unknown as Response);
    const client = createClient();
    await client.postToolResult(
      { request_id: "req1", status: "ok", output: "out" },
      { edgeExecutorId: "edge-1" },
    );
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/tools/result");
    expect((call[1].headers as Record<string, string>)["X-Astra-Edge-Id"]).toBe(
      "edge-1",
    );
  });

  test("postTaskLeaseClaim sends edge header", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () => Promise.resolve("{}"),
      headers: new Headers(),
    } as unknown as Response);
    await createClient().postTaskLeaseClaim(
      "task-1",
      { edge_agent_id: "e1" },
      { edgeTransportId: "transport-1" },
    );
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toContain("/tasks/task-1/lease/claim");
    expect((call[1].headers as Record<string, string>)["X-Astra-Edge-Id"]).toBe(
      "transport-1",
    );
  });
});

// ─── Error handling ────────────────────────────────────────────────

describe("AstraClient — Errors", () => {
  test("throws AstraApiError on non-OK response", async () => {
    globalThis.fetch = mockFetch(404, "Not Found");

    try {
      await createClient().getSession("nonexistent");
      fail("Expected error");
    } catch (e) {
      expect(e).toBeInstanceOf(AstraApiError);
      expect((e as AstraApiError).status).toBe(404);
    }
  });

  test("throws AstraApiError when response is ok but body is not JSON", async () => {
    globalThis.fetch = mockFetch(200, "<!DOCTYPE html><html>");

    try {
      await createClient().getSession("s1");
      fail("Expected error");
    } catch (e) {
      expect(e).toBeInstanceOf(AstraApiError);
      expect((e as AstraApiError).status).toBe(200);
      expect((e as AstraApiError).body).toMatch(/Invalid JSON response/);
    }
  });

  test("merges RequestInit headers (plain record)", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () =>
        Promise.resolve('{"sessions":[], "total":0, "limit":20, "offset":0}'),
      headers: new Headers(),
    } as unknown as Response);

    const client = createClient();
    await client.fetch(PATH_SESSIONS, {
      method: "GET",
      headers: { "X-Plain": "from-record" },
    });

    const headersArg = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][1].headers as Record<string, string>;
    expect(headersArg["X-Plain"]).toBe("from-record");
  });

  test("merges RequestInit headers when value is a Headers object", async () => {
    globalThis.fetch = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () =>
        Promise.resolve('{"sessions":[], "total":0, "limit":20, "offset":0}'),
      headers: new Headers(),
    } as unknown as Response);

    const client = createClient();
    const h = new Headers();
    h.set("X-Custom", "from-headers-object");
    await client.fetch(PATH_SESSIONS, { method: "GET", headers: h });

    const headersArg = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][1].headers as Record<string, string>;
    // Undici/Node may normalize names; value must be present.
    expect(Object.values(headersArg)).toContain("from-headers-object");
    expect(
      Object.keys(headersArg).some((k) => k.toLowerCase() === "x-custom"),
    ).toBe(true);
  });

  test("auto-refresh on 401", async () => {
    const refreshResult = {
      access_token: "refreshed",
      refresh_token: "new-rt",
      token_type: "Bearer",
      expires_in: 3600,
    };
    let callCount = 0;

    globalThis.fetch = vi.fn().mockImplementation((url: string) => {
      callCount++;
      if (callCount === 1) {
        // First call: 401
        return Promise.resolve({
          ok: false,
          status: 401,
          json: () => Promise.resolve({}),
          text: () => Promise.resolve("Unauthorized"),
          headers: new Headers(),
        });
      }
      if (callCount === 2 && url.includes("/auth/refresh")) {
        // Refresh call
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve(refreshResult),
          text: () => Promise.resolve(JSON.stringify(refreshResult)),
          headers: new Headers(),
        });
      }
      // Retry call
      const raw = {
        session_id: "s1",
        user_id: "u1",
        agent_id: null,
        title: null,
        status: "active",
        event_count: 0,
        created_at: "",
        updated_at: null,
        ended_at: null,
        metadata: {},
      };
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve(raw),
        text: () => Promise.resolve(JSON.stringify(raw)),
        headers: new Headers(),
      });
    });

    const onRefresh = vi.fn();
    const client = new AstraClient({
      baseUrl: "http://localhost:8000",
      accessToken: "expired",
      refreshToken: "valid-rt",
      onTokenRefresh: onRefresh,
    });

    const session = await client.getSession("s1");
    expect(session.sessionId).toBe("s1");
    expect(onRefresh).toHaveBeenCalledWith({
      accessToken: "refreshed",
      refreshToken: "new-rt",
    });
  });
});

// ─── chatRequestToWire ─────────────────────────────────────────────

describe("chatRequestToWire", () => {
  test("maps hard allowlists separately from optional tool enablement", () => {
    const body = chatRequestToWire({
      message: "hi",
      selectedModel: { model: "test-model" },
      allowSkills: ["a", "b"],
      allowTools: ["t1"],
      enabledTools: ["web_search", "web_fetch"],
      skillSearch: {
        dynamicSurface: false,
        minCatalogSize: 10,
        surfaceCap: 20,
      },
    });
    expect(body.allow_skills).toEqual(["a", "b"]);
    expect(body.allow_tools).toEqual(["t1"]);
    expect(body.enabled_tools).toEqual(["web_search", "web_fetch"]);
    expect(body.skill_search).toEqual({
      dynamic_surface: false,
      min_catalog_size: 10,
      surface_cap: 20,
    });
  });

  test("omits allow lists when empty", () => {
    const body = chatRequestToWire({
      message: "x",
      selectedModel: { model: "test-model" },
      allowSkills: [],
      allowTools: [],
    });
    expect(body.allow_skills).toBeUndefined();
    expect(body.allow_tools).toBeUndefined();
  });

  test("preserves an empty optional-tool set as an explicit disablement", () => {
    const body = chatRequestToWire({
      message: "x",
      selectedModel: { model: "test-model" },
      enabledTools: [],
    });
    expect(body.enabled_tools).toEqual([]);
  });

  test("default request omits execution_budget", () => {
    const body = chatRequestToWire({
      message: "m",
      selectedModel: { model: "test-model" },
    });
    expect(body.execution_budget).toBeUndefined();
  });

  test("preserves empty selectedModel.gateway for server-side validation", () => {
    const body = chatRequestToWire({
      message: "m",
      selectedModel: { model: "kimi", gateway: "" },
    });

    expect(body.selected_model).toEqual({ model: "kimi", gateway: "" });
  });

  test("preserves selectedModel.id for external runtime context", () => {
    const body = chatRequestToWire({
      message: "m",
      selectedModel: { id: "model-kimi", model: "kimi" },
    });

    expect(body.selected_model).toEqual({ id: "model-kimi", model: "kimi" });
  });

  test("full snake_case mapping: session, agent, selected model, binding, context, plan, edge, capabilities, budget, bindings", () => {
    const body = chatRequestToWire({
      message: "q",
      parts: [{ type: "text", text: "q" }],
      attachments: [{ id: "att-1", kind: "file" }],
      executionBudget: {
        initialTurns: 3,
        hardTurnLimit: 7,
      },
      sessionId: "sess-1",
      agentId: "ag-1",
      selectedModel: { id: "model-kimi", model: "kimi", gateway: "gateway-a" },
      agentBinding: {
        id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
        capabilityServerRefs: {
          mcp: "tools",
          skills: "skills",
        },
      },
      runtimeAuth: {
        authorization: "Bearer runtime-grant",
      },
      runtimeProfile: "agent_binding_registry",
      context: { files: [] },
      explain: true,
      planSubtaskId: "p1",
      isPlanSubtask: true,
      edgeExecutorId: "ex1",
      capabilities: ["a", "b"],
      workspaceBinding: {
        kind: "edge_workspace",
        display_name: "Edge workspace",
        cwd: "/tmp",
        authority: "read_write",
        fallback_policy: "disabled",
      },
      executorBinding: {
        kind: "edge_agent",
        executor_id: "edge-1",
        display_name: "Edge executor",
        status: "online",
        transport: "edge_ws",
      },
    });
    expect(body).toMatchObject({
      message: "q",
      parts: [{ type: "text", text: "q" }],
      attachments: [{ id: "att-1", kind: "file" }],
      execution_budget: {
        initial_turns: 3,
        hard_turn_limit: 7,
      },
      session_id: "sess-1",
      agent_id: "ag-1",
      selected_model: { id: "model-kimi", model: "kimi", gateway: "gateway-a" },
      agent_binding: {
        id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
        capability_server_refs: {
          mcp: "tools",
          skills: "skills",
        },
      },
      runtime_auth: {
        authorization: "Bearer runtime-grant",
      },
      runtime_profile: "agent_binding_registry",
      context: { files: [] },
      explain: true,
      plan_subtask_id: "p1",
      is_plan_subtask: true,
      edge_executor_id: "ex1",
      capabilities: ["a", "b"],
      workspace_binding: {
        kind: "edge_workspace",
        display_name: "Edge workspace",
        cwd: "/tmp",
        authority: "read_write",
        fallback_policy: "disabled",
      },
      executor_binding: {
        kind: "edge_agent",
        executor_id: "edge-1",
        display_name: "Edge executor",
        status: "online",
        transport: "edge_ws",
      },
    });
  });

  test("omits undefined optional fields", () => {
    const body = chatRequestToWire({
      message: "x",
      selectedModel: { model: "test-model" },
    });
    expect(Object.keys(body).sort()).toEqual(
      ["message", "selected_model"].sort(),
    );
  });
});

// ─── Agent Binding Registry ───────────────────────────────────────

describe("AstraClient — Agent Binding registry", () => {
  const createBody = {
    idempotency_key: "idem-1",
    binding: {
      binding_name: "support-agent-v1",
      agent_md: "# Support Agent",
      capability_servers: [
        {
          id: "tools",
          type: "mcp" as const,
          transport: "streamable_http" as const,
          endpoint_url: "https://capabilities.example.com/mcp",
        },
        {
          id: "skills",
          type: "skill" as const,
          transport: "streamable_http" as const,
          endpoint_url: "https://capabilities.example.com/skills",
        },
      ],
      runtime_policy: {
        max_steps: 4,
        tool_mode: "mcp_gateway" as const,
      },
      metadata: { source: "test" },
      binding_schema_version: "v1",
    },
  };

  test("createAgentBinding POST /agent-bindings with body", async () => {
    const response = {
      agent_binding_id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
      binding_name: "support-agent-v1",
      status: "active",
    };
    globalThis.fetch = mockFetch(200, response);

    const client = createClient();
    const out = await client.createAgentBinding(createBody);

    expect(out).toEqual(response);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/agent-bindings");
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual(createBody);
  });

  test("getAgentBinding GET /agent-bindings/{id}", async () => {
    const record = {
      agent_binding_id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
      binding_name: "support-agent-v1",
      status: "active",
      agent_md: "# Support Agent",
      capability_servers: createBody.binding.capability_servers,
      runtime_policy: createBody.binding.runtime_policy,
      metadata: createBody.binding.metadata,
      binding_schema_version: "v1",
      created_at: "2026-06-18T00:00:00",
      disabled_at: null,
    };
    globalThis.fetch = mockFetch(200, record);

    const client = createClient();
    const out = await client.getAgentBinding(record.agent_binding_id);

    expect(out).toEqual(record);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe(
      "http://localhost:8000/agent-bindings/ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
    );
    expect(call[1].method).toBeUndefined();
  });

  test("disableAgentBinding POST /agent-bindings/{id}/disable", async () => {
    const record = {
      agent_binding_id: "ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391",
      binding_name: "support-agent-v1",
      status: "disabled",
      agent_md: "# Support Agent",
      capability_servers: createBody.binding.capability_servers,
      runtime_policy: createBody.binding.runtime_policy,
      metadata: createBody.binding.metadata,
      binding_schema_version: "v1",
      created_at: "2026-06-18T00:00:00",
      disabled_at: "2026-06-18T00:01:00",
    };
    globalThis.fetch = mockFetch(200, record);

    const client = createClient();
    const out = await client.disableAgentBinding(record.agent_binding_id);

    expect(out).toEqual(record);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe(
      "http://localhost:8000/agent-bindings/ab_018f05f5-c7dd-7f43-83e6-93d56d9d7391/disable",
    );
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual({});
  });
});

// ─── Model Gateway Registry ───────────────────────────────────────

describe("AstraClient — Model Gateway registry", () => {
  const createBody = {
    id: "gateway-a",
    resolve_url: "https://models.example.com/resolve",
    model_protocol: "openai_chat_completions" as const,
    metadata: { owner: "platform" },
  };

  test("createModelGateway POST /model-gateways with body", async () => {
    const response = { id: "gateway-a", status: "active" };
    globalThis.fetch = mockFetch(200, response);

    const client = createClient();
    const out = await client.createModelGateway(createBody);

    expect(out).toEqual(response);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/model-gateways");
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual(createBody);
  });

  test("getModelGateway GET /model-gateways/{id}", async () => {
    const record = {
      ...createBody,
      status: "active",
      created_at: "2026-06-18T00:00:00",
      updated_at: "2026-06-18T00:00:00",
      disabled_at: null,
    };
    globalThis.fetch = mockFetch(200, record);

    const client = createClient();
    const out = await client.getModelGateway("gateway-a");

    expect(out).toEqual(record);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/model-gateways/gateway-a");
    expect(call[1].method).toBeUndefined();
  });

  test("disableModelGateway POST /model-gateways/{id}/disable", async () => {
    const record = {
      ...createBody,
      status: "disabled",
      created_at: "2026-06-18T00:00:00",
      updated_at: "2026-06-18T00:01:00",
      disabled_at: "2026-06-18T00:01:00",
    };
    globalThis.fetch = mockFetch(200, record);

    const client = createClient();
    const out = await client.disableModelGateway("gateway-a");

    expect(out).toEqual(record);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe(
      "http://localhost:8000/model-gateways/gateway-a/disable",
    );
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual({});
  });
});

// ─── Skills lifecycle ─────────────────────────────────────────────

describe("AstraClient — Skills lifecycle", () => {
  test("registerSkill POST /skills with body", async () => {
    const rec = {
      skill_id: "id1",
      skill_name: "my-skill",
      version: "1.0.0",
      description: "d",
      metadata: null,
      created_at: "2026-01-01",
    };
    globalThis.fetch = mockFetch(200, rec);

    const client = createClient();
    const out = await client.registerSkill({
      skill_name: "my-skill",
      skill_version: "1.0.0",
      skill_code: "code",
    });
    expect(out).toEqual(rec);
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/skills");
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual({
      skill_name: "my-skill",
      skill_version: "1.0.0",
      skill_code: "code",
    });
  });

  test("publishSkill POST /skills/publish", async () => {
    globalThis.fetch = mockFetch(200, { ok: true });
    const client = createClient();
    await client.publishSkill({
      name: "n",
      version: "1",
      description: "desc",
    });
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe("http://localhost:8000/skills/publish");
    expect(JSON.parse(call[1].body)).toMatchObject({
      name: "n",
      version: "1",
      description: "desc",
    });
  });

  test("getSkill GET /skills/{id} with version query", async () => {
    const rec = {
      skill_id: "n@1.0.0",
      skill_name: "n",
      version: "1.0.0",
      description: null,
      metadata: null,
      created_at: null,
    };
    globalThis.fetch = mockFetch(200, rec);
    const client = createClient();
    const out = await client.getSkill("n@1.0.0", { version: "1.0.0" });
    expect(out).toEqual(rec);
    const url = (globalThis.fetch as ReturnType<typeof vi.fn>).mock
      .calls[0][0] as string;
    expect(url).toContain("/skills/");
    expect(url).toContain(encodeURIComponent("n@1.0.0"));
    expect(url).toContain("version=1.0.0");
  });

  test("unpublishSkill POST /skills/{name}/unpublish", async () => {
    globalThis.fetch = mockFetch(200, {});
    const client = createClient();
    await client.unpublishSkill("my-skill");
    const call = (globalThis.fetch as ReturnType<typeof vi.fn>).mock.calls[0];
    expect(call[0]).toBe(
      `http://localhost:8000/skills/${encodeURIComponent("my-skill")}/unpublish`,
    );
    expect(call[1].method).toBe("POST");
    expect(JSON.parse(call[1].body)).toEqual({});
  });
});
