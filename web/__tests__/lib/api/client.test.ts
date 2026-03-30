// Mock next/headers BEFORE any import
jest.mock('next/headers', () => {
  const mockCookieStore = {
    get: jest.fn(),
    set: jest.fn(),
  };
  return {
    cookies: jest.fn(() => Promise.resolve(mockCookieStore)),
    __mockCookieStore: mockCookieStore,
  };
});

jest.mock('@/lib/runtime-config', () => ({
  getRuntimeConfig: jest.fn(),
  getWebConfigurationMessage: jest.fn(
    () => 'Use the Settings page to configure the runtime API URL and login token, or enable demo mode.',
  ),
  DEFAULT_API_URL: 'http://localhost:8000',
  ACCESS_TOKEN_COOKIE: 'mo_agent_access_token',
  REFRESH_TOKEN_COOKIE: 'mo_agent_refresh_token',
  API_URL_COOKIE: 'mo_agent_api_url',
}));

import { apiFetch, tryApiFetch, apiPost } from '@/lib/api/client';
import { getRuntimeConfig } from '@/lib/runtime-config';

const mockGetRuntimeConfig = getRuntimeConfig as jest.MockedFunction<typeof getRuntimeConfig>;

// Build a valid JWT token with a given sub claim
function fakeJwt(sub: string): string {
  const header = Buffer.from(JSON.stringify({ alg: 'HS256', typ: 'JWT' })).toString('base64url');
  const payload = Buffer.from(JSON.stringify({ sub })).toString('base64url');
  return `${header}.${payload}.fake-signature`;
}

function jsonResponse(data: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? 'OK' : 'Error',
    json: () => Promise.resolve(data),
    text: () => Promise.resolve(JSON.stringify(data)),
    headers: new Map([['content-type', 'application/json']]),
  };
}

describe('API client', () => {
  let fetchMock: jest.Mock;
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    fetchMock = jest.fn().mockResolvedValue(jsonResponse({ ok: true }));
    globalThis.fetch = fetchMock;
    mockGetRuntimeConfig.mockResolvedValue({
      mode: 'live',
      source: 'cookie',
      apiUrl: 'http://api.test:8000',
      accessToken: fakeJwt('user-42'),
      refreshToken: 'refresh-token-val',
      demoMode: false,
      hasAccessToken: true,
      hasRefreshToken: true,
      message: 'Connected',
    });
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
    jest.clearAllMocks();
  });

  // ── apiFetch ────────────────────────────────────────────────────────────
  describe('apiFetch', () => {
    it('makes a GET request to the correct URL', async () => {
      await apiFetch('/agents');
      expect(fetchMock).toHaveBeenCalledWith(
        'http://api.test:8000/agents',
        expect.objectContaining({ headers: expect.any(Object), cache: 'no-store' }),
      );
    });

    it('sets Authorization header from access token', async () => {
      await apiFetch('/agents');
      const call = fetchMock.mock.calls[0];
      expect(call[1].headers['Authorization']).toBe(`Bearer ${fakeJwt('user-42')}`);
    });

    it('sets X-User-Id header from JWT sub claim', async () => {
      await apiFetch('/agents');
      const call = fetchMock.mock.calls[0];
      expect(call[1].headers['X-User-Id']).toBe('user-42');
    });

    it('does not set auth headers when no access token', async () => {
      mockGetRuntimeConfig.mockResolvedValueOnce({
        mode: 'live',
        source: 'cookie',
        apiUrl: 'http://api.test:8000',
        demoMode: false,
        hasAccessToken: false,
        hasRefreshToken: false,
        message: 'No auth',
      });

      await apiFetch('/agents');
      const call = fetchMock.mock.calls[0];
      expect(call[1].headers['Authorization']).toBeUndefined();
      expect(call[1].headers['X-User-Id']).toBeUndefined();
    });

    it('throws on non-ok response', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ detail: 'Not found' }, 404));
      await expect(apiFetch('/missing')).rejects.toThrow('API request failed');
    });

    it('includes error detail from response body', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ detail: 'Session expired' }, 403));
      await expect(apiFetch('/protected')).rejects.toThrow('Session expired');
    });

    it('throws with config message when mode is not live', async () => {
      mockGetRuntimeConfig.mockResolvedValueOnce({
        mode: 'demo',
        source: 'cookie',
        demoMode: true,
        hasAccessToken: false,
        hasRefreshToken: false,
        message: 'Demo mode',
      });

      await expect(apiFetch('/agents')).rejects.toThrow('Settings page');
    });

    it('returns parsed JSON on success', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ data: [1, 2, 3] }));
      const result = await apiFetch<{ data: number[] }>('/data');
      expect(result).toEqual({ data: [1, 2, 3] });
    });

    it('attempts token refresh on 401', async () => {
      // First call returns 401
      fetchMock
        .mockResolvedValueOnce(jsonResponse({}, 401))
        // The refresh call succeeds
        .mockResolvedValueOnce(jsonResponse({ access_token: fakeJwt('user-42'), refresh_token: 'new-refresh' }))
        // Retry succeeds
        .mockResolvedValueOnce(jsonResponse({ ok: true }));

      // Need to have the cookie store return the refresh token
      const nextHeaders = jest.requireMock('next/headers');
      nextHeaders.__mockCookieStore.get.mockImplementation((name: string) => {
        if (name === 'mo_agent_refresh_token') return { value: 'refresh-token-val' };
        if (name === 'mo_agent_api_url') return { value: 'http://api.test:8000' };
        return undefined;
      });

      const result = await apiFetch<{ ok: boolean }>('/protected');
      // Should have made 3 fetch calls: original, refresh, retry
      expect(fetchMock.mock.calls.length).toBe(3);
      expect(result).toEqual({ ok: true });
    });
  });

  // ── tryApiFetch ─────────────────────────────────────────────────────────
  describe('tryApiFetch', () => {
    it('returns data on success', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ items: ['a'] }));
      const result = await tryApiFetch<{ items: string[] }>('/items');
      expect(result).toEqual({ items: ['a'] });
    });

    it('returns null on error', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({}, 500));
      const result = await tryApiFetch('/fail');
      expect(result).toBeNull();
    });

    it('returns null on network error', async () => {
      fetchMock.mockRejectedValueOnce(new Error('Network error'));
      const result = await tryApiFetch('/offline');
      expect(result).toBeNull();
    });
  });

  // ── apiPost ─────────────────────────────────────────────────────────────
  describe('apiPost', () => {
    it('sends POST with JSON body', async () => {
      await apiPost('/sessions', { title: 'New' });
      const call = fetchMock.mock.calls[0];
      expect(call[1].method).toBe('POST');
      expect(call[1].body).toBe(JSON.stringify({ title: 'New' }));
    });

    it('sets correct content-type header', async () => {
      await apiPost('/sessions', { title: 'New' });
      const call = fetchMock.mock.calls[0];
      expect(call[1].headers['Content-Type']).toBe('application/json');
    });

    it('sends POST without body when body is undefined', async () => {
      await apiPost('/trigger');
      const call = fetchMock.mock.calls[0];
      expect(call[1].method).toBe('POST');
      expect(call[1].body).toBeUndefined();
    });

    it('throws on non-ok response', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ error: 'Bad request' }, 400));
      await expect(apiPost('/bad', {})).rejects.toThrow('Bad request');
    });

    it('returns parsed JSON on success', async () => {
      fetchMock.mockResolvedValueOnce(jsonResponse({ id: 'new-1' }));
      const result = await apiPost<{ id: string }>('/create', {});
      expect(result).toEqual({ id: 'new-1' });
    });

    it('throws when mode is not live', async () => {
      mockGetRuntimeConfig.mockResolvedValueOnce({
        mode: 'unconfigured',
        source: 'none',
        demoMode: false,
        hasAccessToken: false,
        hasRefreshToken: false,
        message: 'Not configured',
      });

      await expect(apiPost('/sessions', {})).rejects.toThrow('Settings page');
    });
  });
});
