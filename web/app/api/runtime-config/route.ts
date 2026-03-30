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
  const body = (await request.json()) as RuntimeConfigBody;
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
