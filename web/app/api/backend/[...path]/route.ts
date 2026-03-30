import { NextRequest, NextResponse } from 'next/server';
import { cookies } from 'next/headers';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEFAULT_API_URL,
} from '@/lib/runtime-config';
import {
  buildBackendUrl,
  buildProxyRequestHeaders,
  buildProxyResponseHeaders,
} from '@/lib/api/backend-proxy';

async function proxyRequest(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  const { path } = await context.params;
  const cookieStore = await cookies();
  const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? DEFAULT_API_URL;
  const accessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;
  const url = buildBackendUrl(apiUrl, path, request.nextUrl.search);
  const method = request.method.toUpperCase();

  const init: RequestInit & { duplex?: 'half' } = {
    method,
    headers: buildProxyRequestHeaders(request.headers, accessToken),
    redirect: 'manual',
    cache: 'no-store',
  };

  if (method !== 'GET' && method !== 'HEAD') {
    const body = await request.arrayBuffer();
    if (body.byteLength > 0) {
      init.body = body;
      init.duplex = 'half';
    }
  }

  try {
    const upstream = await fetch(url, init);
    return new NextResponse(upstream.body, {
      status: upstream.status,
      headers: buildProxyResponseHeaders(upstream.headers),
    });
  } catch {
    return NextResponse.json({ error: 'Cannot reach backend' }, { status: 502 });
  }
}

export async function GET(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  return proxyRequest(request, context);
}

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  return proxyRequest(request, context);
}

export async function PUT(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  return proxyRequest(request, context);
}

export async function PATCH(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  return proxyRequest(request, context);
}

export async function DELETE(
  request: NextRequest,
  context: { params: Promise<{ path: string[] }> },
) {
  return proxyRequest(request, context);
}
