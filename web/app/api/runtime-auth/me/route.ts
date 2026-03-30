import { NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE, DEFAULT_API_URL } from '@/lib/runtime-config';

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

  try {
    const res = await fetch(new URL('/auth/me', apiUrl).toString(), {
      headers: { Authorization: `Bearer ${token}` },
      cache: 'no-store',
    });

    if (!res.ok) {
      return NextResponse.json(
        { error: `Backend returned ${res.status}` },
        { status: res.status },
      );
    }

    const user = await res.json();
    return NextResponse.json(user);
  } catch {
    return NextResponse.json({ error: 'Cannot reach backend' }, { status: 502 });
  }
}
