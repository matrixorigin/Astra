import { AstraClient, AstraApiError } from '../client';

// ─── Helpers ────────────────────────────────────────────────────────

function mockFetch(status: number, body: unknown = {}, headers?: Record<string, string>) {
  return jest.fn().mockResolvedValue({
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(body),
    text: () => Promise.resolve(typeof body === 'string' ? body : JSON.stringify(body)),
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

function createClient(token = 'test-access-token') {
  return new AstraClient({
    baseUrl: 'http://localhost:8000',
    accessToken: token,
  });
}

// ─── Auth ─────────────────────────────────────────────────────────

describe('AstraClient — Auth', () => {
  test('login stores tokens and returns result', async () => {
    const authResult = {
      access_token: 'new-at',
      refresh_token: 'new-rt',
      user_id: 'u1',
    };
    globalThis.fetch = mockFetch(200, authResult);

    const client = createClient();
    const result = await client.login('alice', 'pass');

    expect(result).toEqual(authResult);
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[0]).toBe('http://localhost:8000/api/auth/login');
    expect(JSON.parse(call[1].body)).toEqual({ username: 'alice', password: 'pass' });
  });

  test('register stores tokens', async () => {
    const authResult = {
      access_token: 'reg-at',
      refresh_token: 'reg-rt',
      user_id: 'u2',
    };
    globalThis.fetch = mockFetch(200, authResult);

    const client = createClient();
    const result = await client.register('bob', 'pass');

    expect(result).toEqual(authResult);
  });

  test('logout clears tokens', async () => {
    globalThis.fetch = mockFetch(200);

    const client = createClient('tok');
    await client.logout();

    // Next call should have no Authorization header
    globalThis.fetch = mockFetch(200, { user_id: 'u1', username: 'a', created_at: '' });
    await client.getMe().catch(() => {});
    const headers = (globalThis.fetch as jest.Mock).mock.calls[0][1].headers;
    expect(headers['Authorization']).toBeUndefined();
  });

  test('getMe returns user info', async () => {
    const user = { user_id: 'u1', username: 'alice', created_at: '2025-01-01' };
    globalThis.fetch = mockFetch(200, user);

    const client = createClient();
    const result = await client.getMe();
    expect(result).toEqual(user);
  });
});

// ─── Sessions ─────────────────────────────────────────────────────

describe('AstraClient — Sessions', () => {
  test('createSession', async () => {
    const session = { sessionId: 's1', createdAt: '', lastActive: '' };
    globalThis.fetch = mockFetch(200, session);

    const result = await createClient().createSession();
    expect(result.sessionId).toBe('s1');
  });

  test('getSession', async () => {
    const session = { sessionId: 's2', createdAt: '', lastActive: '' };
    globalThis.fetch = mockFetch(200, session);

    const result = await createClient().getSession('s2');
    expect(result.sessionId).toBe('s2');
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('/api/sessions/s2');
  });

  test('listSessions', async () => {
    globalThis.fetch = mockFetch(200, []);
    const result = await createClient().listSessions();
    expect(Array.isArray(result)).toBe(true);
  });

  test('deleteSession', async () => {
    globalThis.fetch = mockFetch(204);
    await createClient().deleteSession('s3');
    const call = (globalThis.fetch as jest.Mock).mock.calls[0];
    expect(call[0]).toContain('/api/sessions/s3');
    expect(call[1].method).toBe('DELETE');
  });

  test('getSessionAudit', async () => {
    const audit = { session_id: 's4', events: [], tool_calls: 3, turns: 2 };
    globalThis.fetch = mockFetch(200, audit);
    const result = await createClient().getSessionAudit('s4');
    expect(result.session_id).toBe('s4');
  });
});

// ─── Runs ─────────────────────────────────────────────────────────

describe('AstraClient — Runs', () => {
  test('createRun', async () => {
    const run = { runId: 'r1', sessionId: 's1', status: 'running', eventsCount: 0 };
    globalThis.fetch = mockFetch(200, run);

    const result = await createClient().createRun({ message: 'hello' });
    expect(result.runId).toBe('r1');
  });

  test('getRunStatus', async () => {
    const run = { runId: 'r1', sessionId: 's1', status: 'completed', eventsCount: 5 };
    globalThis.fetch = mockFetch(200, run);

    const result = await createClient().getRunStatus('r1');
    expect(result.status).toBe('completed');
  });

  test('cancelRun', async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().cancelRun('r1');
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('/api/runs/r1/cancel');
  });

  test('pauseRun', async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().pauseRun('r1');
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('/api/runs/r1/pause');
  });

  test('resumeRun', async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().resumeRun('r1');
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('/api/runs/r1/resume');
  });

  test('getRunEvents', async () => {
    globalThis.fetch = mockFetch(200, []);
    const result = await createClient().getRunEvents('r1');
    expect(Array.isArray(result)).toBe(true);
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('start=0');
  });

  test('getRunEvents with startIndex', async () => {
    globalThis.fetch = mockFetch(200, []);
    await createClient().getRunEvents('r1', 5);
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('start=5');
  });
});

// ─── Memory ─────────────────────────────────────────────────────────

describe('AstraClient — Memory', () => {
  test('memoryStore', async () => {
    globalThis.fetch = mockFetch(200, { id: 'm1' });
    const result = await createClient().memoryStore({ content: 'hello' });
    expect(result.id).toBe('m1');
    const body = JSON.parse((globalThis.fetch as jest.Mock).mock.calls[0][1].body);
    expect(body.content).toBe('hello');
  });

  test('memorySearch', async () => {
    globalThis.fetch = mockFetch(200, [{ id: 'm1', content: 'hello', score: 0.9 }]);
    const result = await createClient().memorySearch('hello');
    expect(result).toHaveLength(1);
    const body = JSON.parse((globalThis.fetch as jest.Mock).mock.calls[0][1].body);
    expect(body.query).toBe('hello');
    expect(body.top_k).toBe(10);
  });

  test('memoryRetrieve', async () => {
    globalThis.fetch = mockFetch(200, []);
    await createClient().memoryRetrieve('query', 3);
    const body = JSON.parse((globalThis.fetch as jest.Mock).mock.calls[0][1].body);
    expect(body.top_k).toBe(3);
  });

  test('memoryPurge', async () => {
    globalThis.fetch = mockFetch(200);
    await createClient().memoryPurge('old-topic');
    const body = JSON.parse((globalThis.fetch as jest.Mock).mock.calls[0][1].body);
    expect(body.topic).toBe('old-topic');
  });
});

// ─── Skills ─────────────────────────────────────────────────────────

describe('AstraClient — Skills', () => {
  test('listSkills', async () => {
    globalThis.fetch = mockFetch(200, []);
    const result = await createClient().listSkills();
    expect(Array.isArray(result)).toBe(true);
    expect((globalThis.fetch as jest.Mock).mock.calls[0][0]).toContain('/api/skills');
  });
});

// ─── Error handling ────────────────────────────────────────────────

describe('AstraClient — Errors', () => {
  test('throws AstraApiError on non-OK response', async () => {
    globalThis.fetch = mockFetch(404, 'Not Found');

    try {
      await createClient().getSession('nonexistent');
      fail('Expected error');
    } catch (e) {
      expect(e).toBeInstanceOf(AstraApiError);
      expect((e as AstraApiError).status).toBe(404);
    }
  });

  test('auto-refresh on 401', async () => {
    const refreshResult = { access_token: 'refreshed', refresh_token: 'new-rt' };
    let callCount = 0;

    globalThis.fetch = jest.fn().mockImplementation((url: string) => {
      callCount++;
      if (callCount === 1) {
        // First call: 401
        return Promise.resolve({
          ok: false,
          status: 401,
          json: () => Promise.resolve({}),
          text: () => Promise.resolve('Unauthorized'),
          headers: new Headers(),
        });
      }
      if (callCount === 2 && url.includes('/auth/refresh')) {
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
      return Promise.resolve({
        ok: true,
        status: 200,
        json: () => Promise.resolve({ sessionId: 's1', createdAt: '', lastActive: '' }),
        text: () => Promise.resolve(''),
        headers: new Headers(),
      });
    });

    const onRefresh = jest.fn();
    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      accessToken: 'expired',
      refreshToken: 'valid-rt',
      onTokenRefresh: onRefresh,
    });

    const session = await client.getSession('s1');
    expect(session.sessionId).toBe('s1');
    expect(onRefresh).toHaveBeenCalledWith({
      accessToken: 'refreshed',
      refreshToken: 'new-rt',
    });
  });
});
