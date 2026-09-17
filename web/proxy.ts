import { NextResponse, type NextRequest } from 'next/server';
import {
  ACCESS_TOKEN_COOKIE,
  REFRESH_TOKEN_COOKIE,
  WEB_CLIENT_ID_COOKIE,
  WEB_CLIENT_ID_HEADER,
} from '@/lib/runtime-config';

const PUBLIC_PAGE_PATHS = new Set(['/', '/login', '/register']);

const PUBLIC_API_PATHS = new Set([
  '/api/runtime-config',
  '/api/runtime-auth/login',
  '/api/runtime-auth/logout',
  '/api/runtime-auth/refresh',
  '/api/runtime-auth/me',
]);

function isStaticAsset(pathname: string): boolean {
  return (
    pathname.startsWith('/_next/') ||
    pathname.startsWith('/fonts/') ||
    pathname === '/favicon.ico' ||
    pathname === '/robots.txt' ||
    pathname === '/sitemap.xml'
  );
}

function isEnabledE2ePath(pathname: string): boolean {
  return process.env.ASTRA_ENABLE_E2E_PAGES === '1' && pathname.startsWith('/e2e/');
}

function hasAuthCredential(request: NextRequest): boolean {
  return (
    request.cookies.has(ACCESS_TOKEN_COOKIE) ||
    request.cookies.has(REFRESH_TOKEN_COOKIE)
  );
}

function isWebClientId(value: string | undefined): value is string {
  return value !== undefined && /^[A-Za-z0-9._:-]{1,128}$/u.test(value);
}

/**
 * Give every browser instance one stable, owner-scoped identity. The value is
 * forwarded on the internal request so the first Work page can use it before
 * the Set-Cookie response is visible to a later refresh. A caller-supplied
 * header is always overwritten; it is never an authorization credential.
 */
function withWebClientIdentity(request: NextRequest): NextResponse {
  const cookieValue = request.cookies.get(WEB_CLIENT_ID_COOKIE)?.value;
  const clientId = isWebClientId(cookieValue) ? cookieValue : crypto.randomUUID();
  const requestHeaders = new Headers(request.headers);
  requestHeaders.set(WEB_CLIENT_ID_HEADER, clientId);
  const response = NextResponse.next({ request: { headers: requestHeaders } });
  if (clientId !== cookieValue) {
    response.cookies.set({
      name: WEB_CLIENT_ID_COOKIE,
      value: clientId,
      httpOnly: true,
      sameSite: 'lax',
      secure: process.env.NODE_ENV === 'production',
      path: '/',
      maxAge: 60 * 60 * 24 * 365,
    });
  }
  return response;
}

function loginRedirect(request: NextRequest): NextResponse {
  const url = request.nextUrl.clone();
  url.pathname = '/login';
  url.search = '';
  const nextPath = `${request.nextUrl.pathname}${request.nextUrl.search}`;
  url.searchParams.set('next', nextPath || '/');
  return NextResponse.redirect(url);
}

export function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;

  if (isStaticAsset(pathname)) {
    return NextResponse.next();
  }

  if (isEnabledE2ePath(pathname)) {
    return NextResponse.next();
  }

  if (PUBLIC_API_PATHS.has(pathname)) {
    return NextResponse.next();
  }

  if (pathname.startsWith('/api/')) {
    if (!hasAuthCredential(request)) {
      return NextResponse.json(
        { error: 'Authentication required.' },
        { status: 401 },
      );
    }
    return NextResponse.next();
  }

  if (PUBLIC_PAGE_PATHS.has(pathname)) {
    return withWebClientIdentity(request);
  }

  if (!hasAuthCredential(request)) {
    return loginRedirect(request);
  }

  return withWebClientIdentity(request);
}

export const config = {
  matcher: [
    // Match all paths except static files and favicon
    '/((?!_next/static|_next/image|favicon.ico).*)',
  ],
};
