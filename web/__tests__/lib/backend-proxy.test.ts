import {
  buildBackendUrl,
  buildProxyRequestHeaders,
  buildProxyResponseHeaders,
} from '@/lib/api/backend-proxy';

describe('backend proxy helpers', () => {
  it('builds backend URLs with path segments and query params', () => {
    expect(
      buildBackendUrl('http://localhost:8000', ['chat', 'runs', 'abc', 'stream'], '?last_index=10'),
    ).toBe('http://localhost:8000/chat/runs/abc/stream?last_index=10');
  });

  it('prefers cookie auth while forwarding only safe request headers', () => {
    const incoming = new Headers({
      accept: 'text/event-stream',
      authorization: 'Bearer stale-client-token',
      'content-type': 'application/json',
      cookie: 'secret=value',
      host: 'example.test',
      'x-trace-id': 'should-not-pass',
    });

    const forwarded = buildProxyRequestHeaders(incoming, 'fresh-cookie-token');

    expect(forwarded.get('accept')).toBe('text/event-stream');
    expect(forwarded.get('content-type')).toBe('application/json');
    expect(forwarded.get('authorization')).toBe('Bearer fresh-cookie-token');
    expect(forwarded.get('cookie')).toBeNull();
    expect(forwarded.get('host')).toBeNull();
    expect(forwarded.get('x-trace-id')).toBeNull();
  });

  it('drops hop-by-hop response headers before returning to the browser', () => {
    const upstream = new Headers({
      'content-type': 'text/event-stream',
      'cache-control': 'no-store',
      connection: 'keep-alive',
      'transfer-encoding': 'chunked',
    });

    const forwarded = buildProxyResponseHeaders(upstream);

    expect(forwarded.get('content-type')).toBe('text/event-stream');
    expect(forwarded.get('cache-control')).toBe('no-store');
    expect(forwarded.get('connection')).toBeNull();
    expect(forwarded.get('transfer-encoding')).toBeNull();
  });
});
