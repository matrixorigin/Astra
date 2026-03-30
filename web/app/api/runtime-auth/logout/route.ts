import { NextResponse } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEMO_MODE_COOKIE, REFRESH_TOKEN_COOKIE, getRuntimeConfig } from '@/lib/runtime-config';

export async function POST() {
  const config = await getRuntimeConfig();
  let backendLogoutError: string | null = null;

  if (config.apiUrl && config.refreshToken) {
    try {
      const response = await fetch(new URL('/auth/logout', config.apiUrl).toString(), {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          refresh_token: config.refreshToken,
        }),
      });

      if (!response.ok) {
        backendLogoutError = `Backend logout failed with ${response.status} ${response.statusText}.`;
      }
    } catch {
      backendLogoutError = 'Backend logout request failed before a response was returned.';
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
