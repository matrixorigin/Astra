import { NextRequest, NextResponse } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEFAULT_API_URL, DEMO_MODE_COOKIE, REFRESH_TOKEN_COOKIE } from '@/lib/runtime-config';

type LoginBody = {
  apiUrl?: string;
  username: string;
  password: string;
};

export async function POST(request: NextRequest) {
  const body = (await request.json()) as LoginBody;
  const apiUrl = body.apiUrl?.trim() || DEFAULT_API_URL;

  if (!body.username || !body.password) {
    return NextResponse.json(
      { error: 'username and password are required.' },
      { status: 400 },
    );
  }

  const response = await fetch(new URL('/auth/login', apiUrl).toString(), {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({
      username: body.username,
      password: body.password,
    }),
  });

  const payload = await response.json().catch(() => ({}));

  if (!response.ok) {
    const detail =
      (payload as { detail?: string; error?: string }).detail ??
      (payload as { detail?: string; error?: string }).error ??
      'Login failed.';

    return NextResponse.json({ error: detail }, { status: response.status });
  }

  const json = payload as {
    access_token: string;
    refresh_token: string;
    expires_in?: number;
    token_type?: string;
  };

  const nextResponse = NextResponse.json({
    ok: true,
    expiresIn: json.expires_in ?? null,
    tokenType: json.token_type ?? 'Bearer',
  });

  nextResponse.cookies.set(API_URL_COOKIE, apiUrl, {
    httpOnly: true,
    sameSite: 'lax',
    path: '/',
  });
  nextResponse.cookies.set(ACCESS_TOKEN_COOKIE, json.access_token, {
    httpOnly: true,
    sameSite: 'lax',
    path: '/',
  });
  nextResponse.cookies.set(REFRESH_TOKEN_COOKIE, json.refresh_token, {
    httpOnly: true,
    sameSite: 'lax',
    path: '/',
  });
  nextResponse.cookies.delete(DEMO_MODE_COOKIE);

  return nextResponse;
}
