import { getRuntimeConfig, getWebConfigurationMessage, type WebDataMode } from '@/lib/runtime-config';

export { getWebConfigurationMessage, type WebDataMode };

export async function getWebDataMode(): Promise<WebDataMode> {
  const config = await getRuntimeConfig();
  return config.mode;
}

/** Extract user ID from a JWT access token (no verification — server validates). */
function extractUserIdFromJwt(token: string): string | null {
  try {
    const payload = JSON.parse(Buffer.from(token.split('.')[1], 'base64url').toString());
    return (payload.sub as string) ?? null;
  } catch {
    return null;
  }
}

/** Try to refresh the access token via the server-side refresh logic. */
async function tryRefreshToken(): Promise<boolean> {
  try {
    // Server-side: directly call the refresh logic using cookies
    const { cookies } = await import('next/headers');
    const cookieStore = await cookies();
    const { REFRESH_TOKEN_COOKIE, API_URL_COOKIE, ACCESS_TOKEN_COOKIE, DEFAULT_API_URL } = await import('@/lib/runtime-config');

    const refreshTokenVal = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;
    const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? DEFAULT_API_URL;

    if (!refreshTokenVal) return false;

    const res = await fetch(new URL('/auth/refresh', apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: refreshTokenVal }),
      cache: 'no-store',
    });

    if (!res.ok) return false;

    const data = (await res.json()) as { access_token: string; refresh_token?: string };
    cookieStore.set(ACCESS_TOKEN_COOKIE, data.access_token, {
      httpOnly: true,
      sameSite: 'lax',
      path: '/',
    });
    if (data.refresh_token) {
      cookieStore.set(REFRESH_TOKEN_COOKIE, data.refresh_token, {
        httpOnly: true,
        sameSite: 'lax',
        path: '/',
      });
    }
    return true;
  } catch {
    return false;
  }
}

function buildHeaders(accessToken?: string): Record<string, string> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (accessToken) {
    headers['Authorization'] = `Bearer ${accessToken}`;
    const userId = extractUserIdFromJwt(accessToken);
    if (userId) {
      headers['X-User-Id'] = userId;
    }
  }
  return headers;
}

export async function apiFetch<T>(path: string): Promise<T> {
  const config = await getRuntimeConfig();

  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error(getWebConfigurationMessage());
  }

  const headers = buildHeaders(config.accessToken);

  let response = await fetch(new URL(path, config.apiUrl).toString(), {
    headers,
    cache: 'no-store',
  });

  // On 401, attempt a token refresh and retry once
  if (response.status === 401 && config.accessToken && config.refreshToken) {
    const refreshed = await tryRefreshToken();
    if (refreshed) {
      // Re-read config to get the new token
      const { getRuntimeConfig: getConfig } = await import('@/lib/runtime-config');
      const newConfig = await getConfig();
      const newHeaders = buildHeaders(newConfig.accessToken);
      response = await fetch(new URL(path, newConfig.apiUrl ?? config.apiUrl).toString(), {
        headers: newHeaders,
        cache: 'no-store',
      });
    }
  }

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;

    try {
      const errorBody = (await response.json()) as { detail?: string; error?: string };
      detail = errorBody.detail ?? errorBody.error ?? detail;
    } catch {
      // Preserve the HTTP status when the body is not JSON.
    }

    throw new Error(`API request failed for ${path}: ${detail}`);
  }

  return (await response.json()) as T;
}

/**
 * Like apiFetch but returns `null` on any API error instead of throwing.
 * Useful for read-only pages that should degrade gracefully.
 */
export async function tryApiFetch<T>(path: string): Promise<T | null> {
  try {
    return await apiFetch<T>(path);
  } catch {
    return null;
  }
}

export async function apiPost<T>(path: string, body?: unknown): Promise<T> {
  const config = await getRuntimeConfig();

  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error(getWebConfigurationMessage());
  }

  const headers = buildHeaders(config.accessToken);

  let response = await fetch(new URL(path, config.apiUrl).toString(), {
    method: 'POST',
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    cache: 'no-store',
  });

  // On 401, attempt a token refresh and retry once
  if (response.status === 401 && config.accessToken && config.refreshToken) {
    const refreshed = await tryRefreshToken();
    if (refreshed) {
      const { getRuntimeConfig: getConfig } = await import('@/lib/runtime-config');
      const newConfig = await getConfig();
      const newHeaders = buildHeaders(newConfig.accessToken);
      response = await fetch(new URL(path, newConfig.apiUrl ?? config.apiUrl).toString(), {
        method: 'POST',
        headers: newHeaders,
        body: body !== undefined ? JSON.stringify(body) : undefined,
        cache: 'no-store',
      });
    }
  }

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;

    try {
      const errorBody = (await response.json()) as { detail?: string; error?: string };
      detail = errorBody.detail ?? errorBody.error ?? detail;
    } catch {
      // Preserve the HTTP status when the body is not JSON.
    }

    throw new Error(`API request failed for POST ${path}: ${detail}`);
  }

  return (await response.json()) as T;
}
