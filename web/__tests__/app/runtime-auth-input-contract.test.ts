// @vitest-environment node

vi.mock('@/lib/auth/runtime-auth-client', () => ({
  runtimeLogin: vi.fn(),
}));

import { NextRequest } from 'next/server';
import { POST as login } from '@/app/api/runtime-auth/login/route';
import { POST as saveRuntimeConfig } from '@/app/api/runtime-config/route';
import { runtimeLogin } from '@/lib/auth/runtime-auth-client';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEMO_MODE_COOKIE,
  DEFAULT_API_URL,
} from '@/lib/runtime-config';

const mockRuntimeLogin = vi.mocked(runtimeLogin);

function request(path: string, body: string) {
  return new NextRequest(`http://web.test${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body,
  });
}

describe('public runtime input contracts', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('returns 400 for malformed login JSON instead of throwing', async () => {
    const response = await login(request('/api/runtime-auth/login', '{'));

    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({ error: 'invalid JSON body' });
    expect(mockRuntimeLogin).not.toHaveBeenCalled();
  });

  it('rejects non-string login credentials before backend I/O', async () => {
    const response = await login(
      request(
        '/api/runtime-auth/login',
        JSON.stringify({ username: 42, password: true }),
      ),
    );

    expect(response.status).toBe(400);
    expect(await response.json()).toEqual({
      error: 'apiUrl, username, and password must be strings',
    });
    expect(mockRuntimeLogin).not.toHaveBeenCalled();
  });

  it('preserves the required-field error for missing login credentials', async () => {
    for (const body of [{}, { password: 'secret' }, { username: 'alice' }]) {
      const response = await login(
        request('/api/runtime-auth/login', JSON.stringify(body)),
      );

      expect(response.status).toBe(400);
      expect(await response.json()).toEqual({
        error: 'username and password are required.',
      });
    }
    expect(mockRuntimeLogin).not.toHaveBeenCalled();
  });

  it('keeps valid login normalization and backend contract', async () => {
    mockRuntimeLogin.mockResolvedValue({
      ok: true,
      data: {
        access_token: 'access',
        refresh_token: 'refresh',
        token_type: 'bearer',
        expires_in: 3600,
      },
    });

    const response = await login(
      request(
        '/api/runtime-auth/login',
        JSON.stringify({ username: 'alice', password: 'secret' }),
      ),
    );

    expect(response.status).toBe(200);
    expect(mockRuntimeLogin).toHaveBeenCalledWith(DEFAULT_API_URL, {
      username: 'alice',
      password: 'secret',
    });
  });

  it('returns 400 for malformed runtime config JSON and wrong field types', async () => {
    const malformed = await saveRuntimeConfig(
      request('/api/runtime-config', '{'),
    );
    expect(malformed.status).toBe(400);
    expect(await malformed.json()).toEqual({ error: 'invalid JSON body' });

    const wrongType = await saveRuntimeConfig(
      request(
        '/api/runtime-config',
        JSON.stringify({ apiUrl: 'http://runtime', demoMode: 'true' }),
      ),
    );
    expect(wrongType.status).toBe(400);
    expect(await wrongType.json()).toEqual({
      error: 'runtime config fields have invalid types',
    });
  });

  it('persists valid runtime config fields as normalized cookies', async () => {
    const response = await saveRuntimeConfig(
      request(
        '/api/runtime-config',
        JSON.stringify({
          apiUrl: ' http://runtime ',
          accessToken: ' access ',
          demoMode: true,
        }),
      ),
    );

    expect(response.status).toBe(200);
    expect(response.cookies.get(API_URL_COOKIE)?.value).toBe('http://runtime');
    expect(response.cookies.get(ACCESS_TOKEN_COOKIE)?.value).toBe('access');
    expect(response.cookies.get(DEMO_MODE_COOKIE)?.value).toBe('true');
  });
});
