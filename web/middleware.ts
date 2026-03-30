import { NextResponse, type NextRequest } from 'next/server';
import { ACCESS_TOKEN_COOKIE } from '@/lib/runtime-config';

// Pages that don't require authentication at all
const PUBLIC_PATHS = new Set(['/login', '/register', '/settings', '/api/runtime-config', '/api/runtime-auth/login', '/api/runtime-auth/logout']);

// Pages that require authentication (write operations, workspace)
const AUTH_REQUIRED_PATHS = ['/workspace'];

function isPublicPath(pathname: string): boolean {
  return PUBLIC_PATHS.has(pathname) || pathname.startsWith('/api/');
}

function requiresAuth(pathname: string): boolean {
  return AUTH_REQUIRED_PATHS.some((p) => pathname.startsWith(p));
}

export function middleware(request: NextRequest) {
  const { pathname } = request.nextUrl;

  // Skip for public paths and static assets
  if (isPublicPath(pathname) || pathname.startsWith('/_next/')) {
    return NextResponse.next();
  }

  // Only require auth for specific protected paths (workspace, etc.)
  // Read-only dashboard pages work without auth
  if (requiresAuth(pathname)) {
    const hasToken = request.cookies.has(ACCESS_TOKEN_COOKIE) || process.env.MO_AGENT_ACCESS_TOKEN;
    if (!hasToken) {
      const url = request.nextUrl.clone();
      url.pathname = '/login';
      url.searchParams.set('next', pathname);
      return NextResponse.redirect(url);
    }
  }

  return NextResponse.next();
}

export const config = {
  matcher: [
    // Match all paths except static files and favicon
    '/((?!_next/static|_next/image|favicon.ico).*)',
  ],
};
