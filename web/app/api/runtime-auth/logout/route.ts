import { NextResponse } from 'next/server';
import { logoutSelection } from '@/lib/auth/browser-session';
import { authErrorResponse } from '@/lib/auth/http-error';

export async function POST() {
  try {
    await logoutSelection();
    return NextResponse.json({ ok: true, backendLogoutError: null });
  } catch (error) { return authErrorResponse(error); }
}
