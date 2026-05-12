import { NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEFAULT_API_URL } from '@/lib/runtime-config';
import { runtimeMe } from '@/lib/auth/runtime-auth-client';

/**
 * Proxy GET /auth/me through Next.js so httpOnly cookies are accessible.
 */
export async function GET() {
  const cookieStore = await cookies();
  const token = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;
  const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? DEFAULT_API_URL;

  if (!token) {
    return NextResponse.json({ error: 'Not authenticated' }, { status: 401 });
  }

  const result = await runtimeMe(apiUrl, token);
  if (!result.ok) {
    return NextResponse.json(
      { error: result.error },
      { status: result.status },
    );
  }

  return NextResponse.json(result.data);
}
