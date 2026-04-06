'use server';

import { cookies } from 'next/headers';
import { redirect } from 'next/navigation';
import {
  API_URL_COOKIE,
  ACCESS_TOKEN_COOKIE,
  REFRESH_TOKEN_COOKIE,
  DEFAULT_API_URL,
} from '@/lib/runtime-config';

type AuthTokens = {
  access_token: string;
  refresh_token: string;
  token_type: string;
  expires_in: number;
};

type AuthUser = {
  user_id: string;
  username: string;
  email: string;
  display_name: string | null;
};

type ActionResult = {
  ok: boolean;
  error?: string;
};

const TOKEN_MAX_AGE = 365 * 24 * 60 * 60; // 1 year for cookie persistence

function getApiUrl(cookieStore: Awaited<ReturnType<typeof cookies>>): string {
  return cookieStore.get(API_URL_COOKIE)?.value ?? process.env.ASTRA_API_URL ?? DEFAULT_API_URL;
}

async function saveTokens(tokens: AuthTokens): Promise<void> {
  const cookieStore = await cookies();
  const opts = {
    httpOnly: true,
    secure: process.env.NODE_ENV === 'production',
    sameSite: 'lax' as const,
    path: '/',
    maxAge: TOKEN_MAX_AGE,
  };

  cookieStore.set(ACCESS_TOKEN_COOKIE, tokens.access_token, opts);
  cookieStore.set(REFRESH_TOKEN_COOKIE, tokens.refresh_token, opts);
}

export async function loginAction(
  _prev: ActionResult,
  formData: FormData,
): Promise<ActionResult> {
  const username = formData.get('username') as string;
  const password = formData.get('password') as string;

  if (!username || !password) {
    return { ok: false, error: 'Username and password are required.' };
  }

  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);

  try {
    const response = await fetch(new URL('/auth/login', apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, password }),
    });

    if (!response.ok) {
      const body = await response.json().catch(() => ({})) as { detail?: string; error?: string };
      return { ok: false, error: body.detail ?? body.error ?? `Login failed: ${response.status}` };
    }

    const tokens = (await response.json()) as AuthTokens;
    await saveTokens(tokens);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Network error' };
  }
}

export async function registerAction(
  _prev: ActionResult,
  formData: FormData,
): Promise<ActionResult> {
  const username = formData.get('username') as string;
  const email = formData.get('email') as string;
  const password = formData.get('password') as string;
  const displayName = (formData.get('display_name') as string) || undefined;

  if (!username || !email || !password) {
    return { ok: false, error: 'Username, email, and password are required.' };
  }

  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);

  try {
    const response = await fetch(new URL('/auth/register', apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ username, email, password, display_name: displayName }),
    });

    if (!response.ok) {
      const body = await response.json().catch(() => ({})) as { detail?: string; error?: string };
      return { ok: false, error: body.detail ?? body.error ?? `Registration failed: ${response.status}` };
    }

    const data = (await response.json()) as AuthTokens & { user_id: string };
    await saveTokens(data);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Network error' };
  }
}

export async function logoutAction(): Promise<void> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;

  // Try server-side logout to revoke tokens
  if (apiUrl && refreshToken) {
    try {
      await fetch(new URL('/auth/logout', apiUrl).toString(), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ refresh_token: refreshToken }),
      });
    } catch {
      // Best effort — still clear cookies
    }
  }

  cookieStore.delete(ACCESS_TOKEN_COOKIE);
  cookieStore.delete(REFRESH_TOKEN_COOKIE);
  redirect('/login');
}

export async function refreshTokenAction(): Promise<ActionResult> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;

  if (!refreshToken) {
    return { ok: false, error: 'No refresh token available.' };
  }

  try {
    const response = await fetch(new URL('/auth/refresh', apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: refreshToken }),
    });

    if (!response.ok) {
      return { ok: false, error: `Token refresh failed: ${response.status}` };
    }

    const tokens = (await response.json()) as AuthTokens;
    await saveTokens(tokens);
    return { ok: true };
  } catch (err) {
    return { ok: false, error: err instanceof Error ? err.message : 'Network error' };
  }
}

export async function getCurrentUser(): Promise<AuthUser | null> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const accessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;

  if (!accessToken) return null;

  try {
    const response = await fetch(new URL('/auth/me', apiUrl).toString(), {
      headers: { Authorization: `Bearer ${accessToken}` },
      cache: 'no-store',
    });

    if (!response.ok) {
      // Try refresh if access token is expired (401)
      if (response.status === 401) {
        const refreshResult = await refreshTokenAction();
        if (!refreshResult.ok) return null;

        // Retry with new token
        const newCookieStore = await cookies();
        const newToken = newCookieStore.get(ACCESS_TOKEN_COOKIE)?.value;
        if (!newToken) return null;

        const retryResponse = await fetch(new URL('/auth/me', apiUrl).toString(), {
          headers: { Authorization: `Bearer ${newToken}` },
          cache: 'no-store',
        });
        if (!retryResponse.ok) return null;
        return (await retryResponse.json()) as AuthUser;
      }
      return null;
    }

    return (await response.json()) as AuthUser;
  } catch {
    return null;
  }
}
