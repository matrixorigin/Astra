import { NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import { getSidebar } from '@/lib/api/web-store';
import type { AuthTokens, AuthUser } from '@/lib/auth/runtime-auth-client';
import { runtimeMe, runtimeRefresh } from '@/lib/auth/runtime-auth-client';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEFAULT_API_URL,
  REFRESH_TOKEN_COOKIE,
} from '@/lib/runtime-config';
import type { UserSummary } from '@/lib/api/types';

const TOKEN_MAX_AGE = 365 * 24 * 60 * 60;

const offlineUser: UserSummary = {
  id: 'offline',
  name: 'Astra user',
  plan: 'free',
};

function toUserSummary(user: AuthUser): UserSummary {
  return {
    id: user.user_id,
    name: user.display_name ?? user.username,
    plan: 'free',
  };
}

async function resolveAuthenticatedUser(): Promise<{
  user: UserSummary;
  refreshedTokens?: AuthTokens;
}> {
  const cookieStore = await cookies();
  const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? process.env.ASTRA_API_URL ?? DEFAULT_API_URL;
  const accessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;

  if (!accessToken) {
    return { user: offlineUser };
  }

  const current = await runtimeMe(apiUrl, accessToken);
  if (current.ok) {
    return { user: toUserSummary(current.data) };
  }

  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;
  if (current.status !== 401 || !refreshToken) {
    return { user: offlineUser };
  }

  const refreshed = await runtimeRefresh(apiUrl, refreshToken);
  if (!refreshed.ok) {
    return { user: offlineUser };
  }

  const retry = await runtimeMe(apiUrl, refreshed.data.access_token);
  if (!retry.ok) {
    return { user: offlineUser };
  }

  return {
    user: toUserSummary(retry.data),
    refreshedTokens: refreshed.data,
  };
}

export async function GET() {
  const auth = await resolveAuthenticatedUser();
  const response = NextResponse.json({
    ...(await getSidebar(auth.user.id)),
    user: auth.user,
  });

  if (auth.refreshedTokens) {
    const cookieOptions = {
      httpOnly: true,
      sameSite: 'lax' as const,
      path: '/',
      maxAge: TOKEN_MAX_AGE,
    };
    response.cookies.set(ACCESS_TOKEN_COOKIE, auth.refreshedTokens.access_token, cookieOptions);
    response.cookies.set(REFRESH_TOKEN_COOKIE, auth.refreshedTokens.refresh_token, cookieOptions);
  }

  return response;
}
