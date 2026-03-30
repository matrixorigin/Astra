import { NextResponse, type NextRequest } from 'next/server';
import { ACCESS_TOKEN_COOKIE, API_URL_COOKIE } from '@/lib/runtime-config';

// Pages that don't require authentication
const PUBLIC_PATHS = new Set(['/login', '/register', '/settings', '/api/runtime-config', '/api/runtime-auth/login', '/api/runtime-auth/logout']);

function isPublicPath(pathname: string): boolean {
  return PUBLIC_PATHS.has(pathname) || pathname.startsWith('/api/');
}

export function middleware(request: NextRequest) {
  const { pathname } = request.nextUrl;

  // Skip auth check for public paths and static assets
  if (isPublicPath(pathname) || pathname.startsWith('/_next/')) {
    return NextResponse.next();
  }

  const hasApiUrl = request.cookies.has(API_URL_COOKIE) || process.env.MO_AGENT_API_URL;
  const hasToken = request.cookies.has(ACCESS_TOKEN_COOKIE) || process.env.MO_AGENT_ACCESS_TOKEN;

  // If no API URL configured, redirect to settings
  if (!hasApiUrl) {
    const url = request.nextUrl.clone();
    url.pathname = '/settings';
    return NextResponse.redirect(url);
  }

  // If no auth token, redirect to login
  if (!hasToken) {
    const url = request.nextUrl.clone();
    url.pathname = '/login';
    url.searchParams.set('next', pathname);
    return NextResponse.redirect(url);
  }

  return NextResponse.next();
}

export const config = {
  matcher: [
    // Match all paths except static files and favicon
    '/((?!_next/static|_next/image|favicon.ico).*)',
  ],
};
