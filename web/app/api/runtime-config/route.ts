import { NextRequest, NextResponse } from 'next/server';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEMO_MODE_COOKIE,
  REFRESH_TOKEN_COOKIE,
  getRuntimeConfig,
} from '@/lib/runtime-config';

type RuntimeConfigBody = {
  apiUrl?: string;
  accessToken?: string;
  refreshToken?: string;
  demoMode?: boolean;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function hasOnlyExpectedFieldTypes(body: Record<string, unknown>): boolean {
  return (
    (body.apiUrl === undefined || typeof body.apiUrl === 'string') &&
    (body.accessToken === undefined || typeof body.accessToken === 'string') &&
    (body.refreshToken === undefined || typeof body.refreshToken === 'string') &&
    (body.demoMode === undefined || typeof body.demoMode === 'boolean')
  );
}

function applyCookie(
  response: NextResponse,
  name: string,
  value: string | undefined,
  httpOnly = true,
) {
  if (value && value.length > 0) {
    response.cookies.set(name, value, {
      httpOnly,
      sameSite: 'lax',
      path: '/',
    });
    return;
  }

  response.cookies.delete(name);
}

export async function GET() {
  const config = await getRuntimeConfig();

  return NextResponse.json({
    mode: config.mode,
    source: config.source,
    apiUrl: config.apiUrl ?? '',
    demoMode: config.demoMode,
    hasAccessToken: config.hasAccessToken,
    hasRefreshToken: config.hasRefreshToken,
    maskedAccessToken: config.maskedAccessToken ?? null,
    message: config.message,
  });
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

  if (!hasOnlyExpectedFieldTypes(rawBody)) {
    return NextResponse.json(
      { error: 'runtime config fields have invalid types' },
      { status: 400 },
    );
  }

  const body = rawBody as RuntimeConfigBody;
  const response = NextResponse.json({ ok: true });

  applyCookie(response, API_URL_COOKIE, body.apiUrl?.trim(), true);
  applyCookie(response, ACCESS_TOKEN_COOKIE, body.accessToken?.trim(), true);
  applyCookie(response, REFRESH_TOKEN_COOKIE, body.refreshToken?.trim(), true);

  if (body.demoMode) {
    response.cookies.set(DEMO_MODE_COOKIE, 'true', {
      httpOnly: true,
      sameSite: 'lax',
      path: '/',
    });
  } else {
    response.cookies.delete(DEMO_MODE_COOKIE);
  }

  return response;
}

export async function DELETE() {
  const response = NextResponse.json({ ok: true });
  response.cookies.delete(API_URL_COOKIE);
  response.cookies.delete(ACCESS_TOKEN_COOKIE);
  response.cookies.delete(REFRESH_TOKEN_COOKIE);
  response.cookies.delete(DEMO_MODE_COOKIE);
  return response;
}
