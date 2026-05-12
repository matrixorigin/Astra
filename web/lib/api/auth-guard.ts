import { NextResponse } from 'next/server';
import { getRuntimeConfig } from '@/lib/runtime-config';
import { getCurrentUser } from '@/lib/auth/actions';

export async function requireRuntimeAuth() {
  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl || !config.hasAccessToken) {
    return NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 });
  }
  return null;
}

export async function requireRuntimeUser() {
  const user = await getCurrentUser();
  if (!user) {
    return {
      user: null,
      response: NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 }),
    };
  }

  return {
    user,
    response: null,
  };
}
