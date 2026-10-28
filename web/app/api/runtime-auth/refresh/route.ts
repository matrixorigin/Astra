import { NextResponse } from 'next/server';
import { selectedSession, resolveBrowserSession } from '@/lib/auth/browser-session';
import { authErrorResponse } from '@/lib/auth/http-error';

export async function POST() {
  try {
    const selection = await selectedSession();
    if (selection?.mode !== 'browser') return NextResponse.json({ error: 'Sign in to refresh your session.' }, { status: 401 });
    await resolveBrowserSession(selection);
    return NextResponse.json({ ok: true });
  } catch (error) { return authErrorResponse(error); }
}
