import { NextRequest, NextResponse } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEFAULT_API_URL, DEMO_MODE_COOKIE, REFRESH_TOKEN_COOKIE } from '@/lib/runtime-config';
import { runtimeLogin } from '@/lib/auth/runtime-auth-client';

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

  const result = await runtimeLogin(apiUrl, {
    username: body.username,
    password: body.password,
  });

  if (!result.ok) {
    return NextResponse.json({ error: result.error }, { status: result.status });
  }

  const json = result.data;

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
