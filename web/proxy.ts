import { NextResponse, type NextRequest } from 'next/server';
import { ACCESS_TOKEN_COOKIE, REFRESH_TOKEN_COOKIE } from '@/lib/runtime-config';

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
    return NextResponse.next();
  }

  if (!hasAuthCredential(request)) {
    return loginRedirect(request);
  }

  return NextResponse.next();
}

export const config = {
  matcher: [
    // Match all paths except static files and favicon
    '/((?!_next/static|_next/image|favicon.ico).*)',
  ],
};
