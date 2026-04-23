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
import { AstraClient } from '../../client';
import { startLocalE2eServer, type LocalE2eServer } from './local-e2e-server';

const e2e = process.env.ASTRA_SDK_E2E === '1';
const describeLocal = e2e && !process.env.ASTRA_SDK_BASE_URL ? describe : describe.skip;
const describeReal = e2e && Boolean(process.env.ASTRA_SDK_BASE_URL) ? describe : describe.skip;

function healthUrlFromBaseUrl(base: string): string {
  return `${base.replace(/\/$/, '')}/health`;
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
