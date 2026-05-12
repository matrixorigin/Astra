import { NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEFAULT_API_URL,
  REFRESH_TOKEN_COOKIE,
} from '@/lib/runtime-config';
import { runtimeRefresh } from '@/lib/auth/runtime-auth-client';

/**
 * Proxy POST /auth/refresh through Next.js so httpOnly refresh_token cookie
 * is accessible server-side. Saves new tokens back as httpOnly cookies.
 */
export async function POST() {
  const cookieStore = await cookies();
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;
  const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? DEFAULT_API_URL;

  if (!refreshToken) {
    return NextResponse.json(
      { error: 'No refresh token available. Please log in again.' },
      { status: 401 },
    );
  }

  const result = await runtimeRefresh(apiUrl, refreshToken);
  if (!result.ok) {
    return NextResponse.json(
      { error: result.error },
      { status: result.status },
    );
  }

  const response = NextResponse.json({ ok: true });
  response.cookies.set(ACCESS_TOKEN_COOKIE, result.data.access_token, {
    httpOnly: true,
    sameSite: 'lax',
    path: '/',
  });
  response.cookies.set(REFRESH_TOKEN_COOKIE, result.data.refresh_token, {
    httpOnly: true,
    sameSite: 'lax',
    path: '/',
  });

  return response;
}
