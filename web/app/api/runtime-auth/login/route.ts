import { NextRequest, NextResponse } from 'next/server';
import { DEFAULT_API_URL } from '@/lib/runtime-config';
import { runtimeLogin } from '@/lib/auth/runtime-auth-client';
import { adoptBrowserSession } from '@/lib/auth/browser-session';
import { SESSION_COOKIE, sessionCookieOptions, runtimeRoot } from '@/lib/auth/session-cookie';
import { authErrorResponse } from '@/lib/auth/http-error';

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

  const apiUrl = body.apiUrl?.trim() || process.env.ASTRA_API_URL || DEFAULT_API_URL;
  try { runtimeRoot(apiUrl); } catch {
    return NextResponse.json({ error: 'Invalid runtime URL.' }, { status: 400 });
  }

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

  try {
    const handle = await adoptBrowserSession(apiUrl, result.data);
    const response = NextResponse.json({ ok: true, expiresIn: result.data.expires_in ?? null, tokenType: result.data.token_type ?? 'Bearer' });
    response.cookies.set(SESSION_COOKIE, handle, sessionCookieOptions());
    return response;
  } catch (error) { return authErrorResponse(error); }
}
