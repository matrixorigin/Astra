import { NextResponse } from 'next/server';
import { getRuntimeConfig } from '@/lib/runtime-config';

export async function requireRuntimeAuth() {
  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl || !config.hasAccessToken) {
    return NextResponse.json({ error: 'AUTH_REQUIRED' }, { status: 401 });
  }
  return null;
}
