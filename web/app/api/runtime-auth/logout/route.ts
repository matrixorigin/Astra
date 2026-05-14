import { NextResponse } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEMO_MODE_COOKIE, REFRESH_TOKEN_COOKIE, getRuntimeConfig } from '@/lib/runtime-config';
import { runtimeLogout } from '@/lib/auth/runtime-auth-client';

export async function POST() {
  const config = await getRuntimeConfig();
  let backendLogoutError: string | null = null;

  if (config.apiUrl && config.refreshToken) {
    const result = await runtimeLogout(config.apiUrl, config.refreshToken);
    if (!result.ok) {
      backendLogoutError = result.error;
    }
  }

  const response = NextResponse.json({
    ok: true,
    backendLogoutError,
  });
  response.cookies.delete(ACCESS_TOKEN_COOKIE);
  response.cookies.delete(REFRESH_TOKEN_COOKIE);
  response.cookies.delete(DEMO_MODE_COOKIE);
  response.cookies.delete(API_URL_COOKIE);
  return response;
}
