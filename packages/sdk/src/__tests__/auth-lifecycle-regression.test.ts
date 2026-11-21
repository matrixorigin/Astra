import { AstraClient } from '../client';

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const unauthorized = () => Response.json({ detail: 'expired' }, { status: 401 });
const tokens = { access_token: 'new-access', refresh_token: 'new-refresh' };
const bearer = (init?: RequestInit) => new Headers(init?.headers).get('authorization');
afterEach(() => vi.unstubAllGlobals());

test('replacing only the access token cannot refresh as the previous account', async () => {
  const fetch = vi.fn(async () => unauthorized());
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'alice', refreshToken: 'alice-refresh' });
  client.setTokens('bob');
  await expect(client.getMe()).rejects.toMatchObject({ status: 401 });
  expect(client.getTokens()).toEqual({ accessToken: 'bob', refreshToken: null });
  expect(fetch).toHaveBeenCalledTimes(1);
});

test.each([204, 500])('late logout response %i cannot erase a newer login', async (status) => {
  const logout = deferred<Response>();
  vi.stubGlobal('fetch', vi.fn(async (url: string) => {
    if (url.endsWith('/auth/logout')) return logout.promise;
    if (url.endsWith('/auth/login')) return Response.json(tokens);
    throw new Error(url);
  }));
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const pending = client.logout().then(() => 'ok', () => 'error');
  expect(client.getTokens()).toEqual({ accessToken: null, refreshToken: null });
  await client.login('bob', 'password');
  logout.resolve(status === 204 ? new Response(null, { status }) : new Response('failed', { status }));
  expect(await pending).toBe(status === 204 ? 'ok' : 'error');
  expect(client.getTokens()).toEqual({ accessToken: 'new-access', refreshToken: 'new-refresh' });
});

test.each(['login', 'register'] as const)('pending %s cannot restore a session after logout', async (method) => {
  const result = deferred<Response>();
  vi.stubGlobal('fetch', vi.fn(async () => result.promise));
  const client = new AstraClient({ baseUrl: 'https://example.test' });
  const pending = client[method]('bob', 'password');
  await client.logout();
  result.resolve(Response.json(tokens));
  await expect(pending).rejects.toThrow('superseded');
  expect(client.getTokens()).toEqual({ accessToken: null, refreshToken: null });
});

test('a slower login cannot overwrite a later successful login', async () => {
  const first = deferred<Response>();
  let calls = 0;
  vi.stubGlobal('fetch', vi.fn(async () => ++calls === 1 ? first.promise : Response.json(tokens)));
  const client = new AstraClient({ baseUrl: 'https://example.test' });
  const oldLogin = client.login('alice', 'password');
  await client.login('bob', 'password');
  first.resolve(Response.json({ access_token: 'alice', refresh_token: 'alice-refresh' }));
  await expect(oldLogin).rejects.toThrow('superseded');
  expect(client.getTokens()).toEqual({ accessToken: 'new-access', refreshToken: 'new-refresh' });
});

test.each(['login', 'register'] as const)('failed %s does not discard an in-flight refresh of the current account', async (method) => {
  const refresh = deferred<Response>();
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/auth/refresh')) return refresh.promise;
    if (url.endsWith('/auth/login') || url.endsWith('/auth/register')) return unauthorized();
    return bearer(init) === 'Bearer new-access' ? Response.json({ user_id: 'alice' }) : unauthorized();
  }));
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const me = client.getMe();
  await vi.waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(2));
  await expect(client[method]('bob', 'bad-password')).rejects.toMatchObject({ status: 401 });
  refresh.resolve(Response.json(tokens));
  await expect(me).resolves.toEqual({ user_id: 'alice' });
  expect(client.getTokens()).toEqual({ accessToken: 'new-access', refreshToken: 'new-refresh' });
});

test.each([
  { authorization: 'Bearer external' },
  new Headers({ AUTHORIZATION: 'Bearer external' }),
  { forEach: (callback: (value: string, key: string) => void) => callback('Bearer external', 'authorization') } as Headers,
])('explicit authorization is not refreshed using client credentials', async (headers) => {
  const fetch = vi.fn(async () => unauthorized());
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const response = await client.fetchResponse('/resource', { headers });
  expect(response.status).toBe(401);
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(client.getTokens()).toEqual({ accessToken: 'old', refreshToken: 'old-refresh' });
});

test('raw mutations require explicit retry and a replayable body', async () => {
  let refreshes = 0;
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/auth/refresh')) { refreshes++; return Response.json(tokens); }
    return bearer(init) === 'Bearer new-access' ? new Response('ok') : unauthorized();
  });
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  expect((await client.fetchResponse('/write', { method: 'POST', body: '{}' })).status).toBe(401);
  expect(refreshes).toBe(0);
  expect((await client.fetchResponse('/write', { method: 'POST', body: '{}' }, { retryUnauthorized: true })).status).toBe(200);
  expect(refreshes).toBe(1);
  client.setTokens('old', 'old-refresh');
  const body = new ReadableStream({ start(controller) { controller.close(); } });
  expect((await client.fetchResponse('/write', { method: 'POST', body }, { retryUnauthorized: true })).status).toBe(401);
  expect(refreshes).toBe(1);
});

test.each(['chat', 'work'] as const)('%s stream and REST share refresh without replaying an accepted stream', async (kind) => {
  const refresh = deferred<Response>();
  let refreshes = 0;
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/auth/refresh')) { refreshes++; return refresh.promise; }
    if (bearer(init) === 'Bearer old') return unauthorized();
    if (url.endsWith('/auth/me')) return Response.json({ user_id: 'alice' });
    return new Response("data: {\"type\":\"run_finished\",\"run_id\":\"run-1\",\"owner_generation\":4,\"status\":\"completed\"}\n\ndata: [DONE]\n\n");
  });
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const events: unknown[] = [];
  if (kind === 'chat') client.streamChat({ message: 'hi', modelSelection: { offeringId: 'offer' } }, { onEvent: event => events.push(event) });
  else client.continueWorkBranch('work-1', 'branch-1', { requestId: 'request-1', attachmentId: 'attachment-1', message: 'hi' }, { onEvent: event => events.push(event) });
  const rest = client.getMe();
  await vi.waitFor(() => expect(refreshes).toBe(1));
  refresh.resolve(Response.json(tokens));
  await expect(rest).resolves.toEqual({ user_id: 'alice' });
  await vi.waitFor(() => expect(events).toHaveLength(1));
  expect(events[0]).toMatchObject({ type: 'run_finished' });
  expect(refreshes).toBe(1);
  expect(fetch.mock.calls.filter(([url]) => url.includes(kind === 'chat' ? '/chat/stream' : '/turns'))).toHaveLength(2);
});

test('an aborted refresh waiter settles while other callers keep sharing the refresh', async () => {
  const refresh = deferred<Response>();
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/auth/refresh')) return refresh.promise;
    return bearer(init) === 'Bearer new-access' ? new Response('ok') : unauthorized();
  });
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const controller = new AbortController();
  const aborted = client.fetchResponse('/cancelled', { signal: controller.signal });
  const other = client.fetchResponse('/other');
  await vi.waitFor(() => expect(fetch.mock.calls.filter(([url]) => url.endsWith('/auth/refresh'))).toHaveLength(1));
  controller.abort();
  // This must settle without releasing the pending shared refresh.
  await expect(aborted).rejects.toMatchObject({ name: 'AbortError' });
  expect(fetch.mock.calls.filter(([url]) => url.endsWith('/cancelled'))).toHaveLength(1);
  refresh.resolve(Response.json(tokens));
  expect((await other).status).toBe(200);
  expect(fetch.mock.calls.filter(([url]) => url.endsWith('/auth/refresh'))).toHaveLength(1);
});

test('an abort between refresh completion and replay preserves the caller reason', async () => {
  const controller = new AbortController();
  const reason = new Error('caller stopped');
  const fetch = vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/auth/refresh')) return Response.json(tokens);
    if (bearer(init) === 'Bearer new-access') {
      if (url.endsWith('/first')) controller.abort(reason);
      return new Response('ok');
    }
    return unauthorized();
  });
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const first = client.fetchResponse('/first');
  const second = client.fetchResponse('/second', { signal: controller.signal });
  await expect(first).resolves.toMatchObject({ status: 200 });
  await expect(second).rejects.toBe(reason);
  expect(fetch.mock.calls.filter(([url]) => url.endsWith('/second'))).toHaveLength(1);
});

test('aborted waiters release their registrations even when refresh never settles', async () => {
  const refresh = deferred<Response>();
  vi.stubGlobal('fetch', vi.fn(async (url: string) => url.endsWith('/auth/refresh') ? refresh.promise : unauthorized()));
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const reason = new Error('caller stopped');
  for (let i = 0; i < 20; i++) {
    const controller = new AbortController();
    const removed = vi.spyOn(controller.signal, 'removeEventListener');
    const request = client.fetchResponse('/resource', { signal: controller.signal });
    await vi.waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(i + 2));
    controller.abort(reason);
    await expect(request).rejects.toBe(reason);
    expect(removed).toHaveBeenCalledWith('abort', expect.any(Function));
  }
  const remaining = client.fetchResponse('/remaining');
  await vi.waitFor(() => expect(vi.mocked(fetch)).toHaveBeenCalledTimes(22));
  refresh.resolve(Response.json(tokens));
  expect((await remaining).status).toBe(401);
  expect(vi.mocked(fetch).mock.calls.filter(([url]) => String(url).endsWith('/auth/refresh'))).toHaveLength(1);
});

test('closing a stream while refresh is pending prevents a second POST', async () => {
  const refresh = deferred<Response>();
  const fetch = vi.fn(async (url: string) => url.endsWith('/auth/refresh') ? refresh.promise : unauthorized());
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'old', refreshToken: 'old-refresh' });
  const stream = client.streamChat({ message: 'hi', modelSelection: { offeringId: 'offer' } }, { onEvent: () => {} });
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(2));
  stream.close();
  refresh.resolve(Response.json(tokens));
  await vi.waitFor(() => expect(client.getTokens().accessToken).toBe('new-access'));
  expect(fetch.mock.calls.filter(([url]) => url.endsWith('/chat/stream'))).toHaveLength(1);
});

test('reconnecting an old stream cannot issue a request under a replacement account', async () => {
  const fetch = vi.fn(async () => new Response("data: {\"type\":\"run_finished\",\"run_id\":\"run-1\",\"owner_generation\":4,\"status\":\"completed\"}\n\ndata: [DONE]\n\n"));
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'alice' });
  const events: unknown[] = [];
  const stream = client.streamChat({ message: 'hi', modelSelection: { offeringId: 'offer' } }, { onEvent: event => events.push(event) });
  await vi.waitFor(() => expect(events).toHaveLength(1));
  client.setTokens('bob');
  await stream.connect();
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(events[1]).toMatchObject({ type: 'error', retryable: false });
});

test('stream requests preserve configured headers while using the current owned bearer', async () => {
  const fetch = vi.fn(async (_url: string, _init?: RequestInit) => new Response("data: {\"type\":\"run_finished\",\"run_id\":\"run-1\",\"owner_generation\":4,\"status\":\"completed\"}\n\ndata: [DONE]\n\n"));
  vi.stubGlobal('fetch', fetch);
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'current', headers: {
    Authorization: 'Bearer configured', Accept: 'application/custom', 'Content-Type': 'application/custom+json',
  } });
  const events: unknown[] = [];
  client.streamChat({ message: 'hi', modelSelection: { offeringId: 'offer' } }, { onEvent: event => events.push(event) });
  await vi.waitFor(() => expect(events).toHaveLength(1));
  const headers = new Headers(fetch.mock.calls[0][1]?.headers);
  expect(headers.get('authorization')).toBe('Bearer current');
  expect(headers.get('accept')).toBe('application/custom');
  expect(headers.get('content-type')).toBe('application/custom+json');
});
