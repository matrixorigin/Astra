/**
 * Real-fetch integration tests.
 *
 * **Mode A** — `ASTRA_SDK_E2E=1` without `ASTRA_SDK_BASE_URL` → in-process [local-e2e-server.ts](./local-e2e-server.ts).
 *
 * **Mode B** — `ASTRA_SDK_E2E=1` with `ASTRA_SDK_BASE_URL` → your running API. Set one of:
 * - `ASTRA_SDK_ACCESS_TOKEN`, or
 * - `ASTRA_SDK_USERNAME` + `ASTRA_SDK_PASSWORD`
 * for authenticated tests (`getMe`, `listSessions`, …). Optional: `ASTRA_SDK_PATH_PREFIX`, `ASTRA_SDK_TEST_RUN_ID`.
 */
import { AstraApiError, AstraClient } from '../../client';
import type { StreamEvent } from '../../types';
import { startLocalE2eServer, type LocalE2eServer } from './local-e2e-server';

const e2e = process.env.ASTRA_SDK_E2E === '1';
const describeLocal = e2e && !process.env.ASTRA_SDK_BASE_URL ? describe : describe.skip;
const describeReal = e2e && Boolean(process.env.ASTRA_SDK_BASE_URL) ? describe : describe.skip;

function healthUrlFromBaseUrl(base: string): string {
  return `${base.replace(/\/$/, '')}/health`;
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const started = Date.now();
  while (!predicate()) {
    if (Date.now() - started > timeoutMs) {
      throw new Error('timed out waiting for local e2e condition');
    }
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}

describeLocal('integration / online (Mode A: local HTTP harness)', () => {
  let harness: LocalE2eServer;
  let prefixHarness: LocalE2eServer;

  beforeAll(async () => {
    harness = await startLocalE2eServer('');
    prefixHarness = await startLocalE2eServer('/api');
  }, 20_000);

  afterAll(async () => {
    await harness.close();
    await prefixHarness.close();
  }, 20_000);

  test('GET /health returns 2xx (root path, not pathPrefix)', async () => {
    const res = await fetch(healthUrlFromBaseUrl(harness.baseUrl));
    expect(res.ok).toBe(true);
  });

  test('AstraClient login + getMe', async () => {
    const client = new AstraClient({ baseUrl: harness.baseUrl });
    await client.login(harness.testUsername, harness.testPassword);
    const me = await client.getMe();
    expect(me.user_id).toBe('harness-uid-1');
  });

  test('AstraClient getMe with bearer token (no prior login in this client)', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const me = await client.getMe();
    expect(me.username).toBe(harness.testUsername);
  });

  test('listSessions returns empty', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const list = await client.listSessions();
    expect(list).toEqual([]);
  });

  test('getRunStatus normalizes run wire', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const s = await client.getRunStatus('run-h1');
    expect(s.runId).toBe('run-h1');
    expect(s.status).toBe('completed');
  });

  test('getRunEvents parses buffered SSE and sends last_index', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const evs = await client.getRunEvents('run-h2', 4);
    expect(evs.map((e) => e.type)).toEqual(['text_delta', 'turn_complete']);
  });

  test('multi-turn streamChat preserves session context and replays persisted run events', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const session = await client.createSession({
      title: 'local multi-turn',
      metadata: { scenario: 'context-regression' },
    });

    const firstEvents: StreamEvent[] = [];
    const first = client.streamChat(
      {
        sessionId: session.sessionId,
        message: 'turn one',
        context: { memory: ['prefer PKCE'], turn: 1 },
        model: 'harness-model',
      },
      { onEvent: (event) => firstEvents.push(event) },
    );
    await waitFor(() => firstEvents.some((event) => event.type === 'turn_complete'));
    first.close();

    const firstRunId = firstEvents.find((event) => event.type === 'session_info')?.run_id;
    expect(firstRunId).toBeDefined();
    const replay = await client.getRunEvents(firstRunId!, 2);
    expect(replay.map((event) => event.type)).toEqual(['text_delta', 'usage', 'turn_complete']);
    expect(replay[0]).toMatchObject({
      type: 'text_delta',
      content: expect.stringContaining('"prefer PKCE"'),
    });

    const secondEvents: StreamEvent[] = [];
    const second = client.streamChat(
      {
        sessionId: session.sessionId,
        message: 'turn two',
        context: { memory: ['prefer PKCE', 'schema changed'], turn: 2 },
      },
      { onEvent: (event) => secondEvents.push(event) },
    );
    await waitFor(() => secondEvents.some((event) => event.type === 'turn_complete'));
    second.close();

    expect(secondEvents.find((event) => event.type === 'text_delta')).toMatchObject({
      type: 'text_delta',
      content: expect.stringContaining('turn=2'),
    });
    expect(secondEvents.find((event) => event.type === 'usage')).toMatchObject({
      type: 'usage',
      cache_read_tokens: 100,
    });

    const audit = await client.getSessionAudit(session.sessionId);
    expect(audit.turn_count).toBe(2);
    expect(audit.status).toBe('active');
    const activity = await client.getSessionActivity(session.sessionId);
    expect(activity.total).toBeGreaterThanOrEqual(10);
  });

  test('closed session rejects a follow-up stream with a surfaced error event', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const session = await client.createSession({ title: 'close guard' });
    await client.closeSession(session.sessionId);

    const events: StreamEvent[] = [];
    const sse = client.streamChat(
      { sessionId: session.sessionId, message: 'should fail' },
      { onEvent: (event) => events.push(event) },
    );
    await waitFor(() => events.some((event) => event.type === 'error'));
    sse.close();

    expect(events[0]).toMatchObject({
      type: 'error',
      message: expect.stringContaining('session is closed'),
      retryable: false,
    });
  });

  test('memory store/search/retrieve/purge runs through local HTTP state', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    const semantic = await client.memoryStore({
      content: 'OAuth migration prefers PKCE and preserves sessions',
      memory_type: 'semantic',
      session_id: 'sess-memory-http',
      trust_tier: 'T2',
    });
    await client.memoryStore({
      content: 'Cloud-edge callbacks are idempotent by request id',
      memory_type: 'procedural',
      session_id: 'sess-memory-http',
    });

    expect(semantic.id).toMatch(/^mem-/);
    const search = await client.memorySearch('OAuth PKCE', 1);
    expect(search).toHaveLength(1);
    expect(search[0]).toMatchObject({ id: semantic.id, score: 1 });

    const retrieved = await client.memoryRetrieve('callbacks request id', 5);
    expect(retrieved.map((memory) => memory.content)).toContain(
      'Cloud-edge callbacks are idempotent by request id',
    );

    await client.memoryPurge('OAuth');
    const afterPurge = await client.memorySearch('OAuth PKCE', 5);
    expect(afterPurge).toEqual([]);
  });

  test('memory purge rejects an empty topic without deleting unrelated memories', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });
    await client.memoryStore({
      content: 'Do not delete this unrelated memory',
      memory_type: 'semantic',
    });

    await expect(client.memoryPurge('')).rejects.toBeInstanceOf(AstraApiError);
    const stillThere = await client.memorySearch('unrelated memory', 5);
    expect(stillThere.map((memory) => memory.content)).toContain(
      'Do not delete this unrelated memory',
    );
  });

  test('delegation lifecycle persists child runs across delegate/list/pause/resume', async () => {
    const client = new AstraClient({
      baseUrl: harness.baseUrl,
      accessToken: harness.staticAccessToken,
    });

    const result = await client.delegateRun('run-parent-http', {
      delegation_id: 'del-http',
      parent_run_id: 'run-parent-http',
      task: 'fan out to cloud and edge agents',
      pattern: { fan_out: { agent_ids: ['agent-cloud', 'agent-edge'] } },
      user_id: 'harness-uid-1',
      depth: 1,
      context: { session_id: 'sess-delegation-http' },
    });
    expect(result.status).toBe('completed');
    expect(result.agent_results.map((agent) => agent.agent_id)).toEqual([
      'agent-cloud',
      'agent-edge',
    ]);

    const listed = await client.listDelegations('run-parent-http');
    expect(listed.sub_run_ids).toEqual([
      'run-parent-http-agent-cloud-child',
      'run-parent-http-agent-edge-child',
    ]);
    await expect(client.pauseDelegations('run-parent-http')).resolves.toEqual({
      parent_run_id: 'run-parent-http',
      affected: 2,
    });
    await expect(client.resumeDelegations('run-parent-http')).resolves.toEqual({
      parent_run_id: 'run-parent-http',
      affected: 2,
    });
  });

  test('pathPrefix: same routes under /api + client.pathPrefix', async () => {
    const c = new AstraClient({
      baseUrl: prefixHarness.baseUrl,
      pathPrefix: '/api',
      accessToken: prefixHarness.staticAccessToken,
    });
    const me = await c.getMe();
    expect(me.user_id).toBe('harness-uid-1');
  });
});

describeReal('integration / online (Mode B: remote ASTRA_SDK_BASE_URL)', () => {
  const rawBase = process.env.ASTRA_SDK_BASE_URL;
  if (!rawBase) {
    it.skip('Mode B requires ASTRA_SDK_BASE_URL (set when running this block)', () => {});
    return;
  }
  const baseUrl = rawBase.replace(/\/$/, '');
  const pathPrefix = (process.env.ASTRA_SDK_PATH_PREFIX || '').replace(/\/$/, '') || undefined;
  const testRunId = process.env.ASTRA_SDK_TEST_RUN_ID;

  const hasToken = Boolean(process.env.ASTRA_SDK_ACCESS_TOKEN);
  const hasLoginCreds = Boolean(
    process.env.ASTRA_SDK_USERNAME && process.env.ASTRA_SDK_PASSWORD,
  );
  const hasAuth = hasToken || hasLoginCreds;

  test('GET /health returns 2xx (server root; not under pathPrefix)', async () => {
    const res = await fetch(healthUrlFromBaseUrl(baseUrl));
    expect(res.ok).toBe(true);
  }, 20_000);

  const needAuth = hasAuth ? describe : describe.skip;
  needAuth('authenticated API (ASTRA_SDK_ACCESS_TOKEN or USERNAME+PASSWORD)', () => {
    let client: AstraClient;

    beforeAll(async () => {
      if (hasToken) {
        client = new AstraClient({
          baseUrl,
          pathPrefix,
          accessToken: process.env.ASTRA_SDK_ACCESS_TOKEN!,
        });
      } else {
        client = new AstraClient({ baseUrl, pathPrefix });
        await client.login(
          process.env.ASTRA_SDK_USERNAME!,
          process.env.ASTRA_SDK_PASSWORD!,
        );
      }
    }, 60_000);

    test('getMe', async () => {
      const me = await client.getMe();
      expect(me.user_id).toBeDefined();
    }, 20_000);

    test('listSessions', async () => {
      const list = await client.listSessions();
      expect(Array.isArray(list)).toBe(true);
    }, 20_000);

    const withRun = testRunId ? test : test.skip;
    withRun('getRunStatus (ASTRA_SDK_TEST_RUN_ID)', async () => {
      const s = await client.getRunStatus(testRunId!);
      expect(s.runId).toBe(testRunId);
    }, 30_000);

    withRun('getRunEvents (ASTRA_SDK_TEST_RUN_ID, last_index=0)', async () => {
      const evs = await client.getRunEvents(testRunId!, 0);
      expect(Array.isArray(evs)).toBe(true);
    }, 60_000);
  });
});
