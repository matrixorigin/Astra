import { NextRequest, NextResponse } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEFAULT_API_URL, DEMO_MODE_COOKIE, REFRESH_TOKEN_COOKIE } from '@/lib/runtime-config';
import { runtimeLogin } from '@/lib/auth/runtime-auth-client';

type LoginBody = {
  apiUrl?: string;
  username: string;
  password: string;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export async function POST(request: NextRequest) {
  let rawBody: unknown;
  try {
    rawBody = await request.json();
  } catch {
    return NextResponse.json({ error: 'invalid JSON body' }, { status: 400 });
  }

  if (!isRecord(rawBody)) {
    return NextResponse.json(
      { error: 'request body must be a JSON object' },
      { status: 400 },
    );
  }

  const body = rawBody as Partial<LoginBody>;
  if (body.username === undefined || body.password === undefined) {
    return NextResponse.json(
      { error: 'username and password are required.' },
      { status: 400 },
    );
  }

  if (
    (body.apiUrl !== undefined && typeof body.apiUrl !== 'string') ||
    typeof body.username !== 'string' ||
    typeof body.password !== 'string'
  ) {
    return NextResponse.json(
      { error: 'apiUrl, username, and password must be strings' },
      { status: 400 },
    );
  }

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
