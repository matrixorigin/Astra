const REQUEST_HEADER_ALLOWLIST = [
  'accept',
  'content-type',
  'if-none-match',
  'if-match',
  'if-modified-since',
  'if-unmodified-since',
  'prefer',
  'range',
] as const;

const RESPONSE_HEADER_BLOCKLIST = new Set([
  'connection',
  'content-length',
  'content-encoding',
  'keep-alive',
  'proxy-authenticate',
  'proxy-authorization',
  'te',
  'trailer',
  'transfer-encoding',
  'upgrade',
]);

export function buildBackendUrl(
  apiUrl: string,
  pathSegments: string[],
  search: string,
): string {
  const base = apiUrl.endsWith('/') ? apiUrl : `${apiUrl}/`;
  const target = new URL(pathSegments.join('/'), base);
  target.search = search;
  return target.toString();
}

export function buildProxyRequestHeaders(
  incomingHeaders: Headers,
  accessToken?: string,
): Headers {
  const headers = new Headers();

  for (const name of REQUEST_HEADER_ALLOWLIST) {
    const value = incomingHeaders.get(name);
    if (value) {
      headers.set(name, value);
    }
  }

  const forwardedAuth = accessToken
    ? `Bearer ${accessToken}`
    : incomingHeaders.get('authorization');
  if (forwardedAuth) {
    headers.set('authorization', forwardedAuth);
  }

  return headers;
}

export function buildProxyResponseHeaders(upstreamHeaders: Headers): Headers {
  const headers = new Headers();

  upstreamHeaders.forEach((value, key) => {
    if (!RESPONSE_HEADER_BLOCKLIST.has(key.toLowerCase())) {
      headers.set(key, value);
    }
  });

  return headers;
}
