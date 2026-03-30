import { NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEFAULT_API_URL,
  REFRESH_TOKEN_COOKIE,
} from '@/lib/runtime-config';

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

  try {
    const res = await fetch(new URL('/auth/refresh', apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: refreshToken }),
      cache: 'no-store',
    });

    if (!res.ok) {
      return NextResponse.json(
        { error: 'Token refresh failed. Please log in again.' },
        { status: res.status },
      );
    }

    const data = (await res.json()) as {
      access_token: string;
      refresh_token?: string;
    };

    const response = NextResponse.json({ ok: true });

    response.cookies.set(ACCESS_TOKEN_COOKIE, data.access_token, {
      httpOnly: true,
      sameSite: 'lax',
      path: '/',
    });

    if (data.refresh_token) {
      response.cookies.set(REFRESH_TOKEN_COOKIE, data.refresh_token, {
        httpOnly: true,
        sameSite: 'lax',
        path: '/',
      });
    }

    return response;
  } catch {
    return NextResponse.json(
      { error: 'Token refresh failed. Network error.' },
      { status: 502 },
    );
  }
}
