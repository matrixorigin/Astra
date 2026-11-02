import { createHash } from 'node:crypto';
import { runInNewContext } from 'node:vm';
// @vitest-environment node
import { NextRequest } from 'next/server';
import { GET, POST } from '@/app/api/personal-models/route';
import { GET as callback } from '@/app/auth/callback/route';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
vi.mock('@/lib/api/auth-guard', () => ({ requireRuntimeUser: vi.fn() }));
const auth = vi.mocked(requireRuntimeUser);
const binding = createHash('sha256').update(JSON.stringify(['http://runtime.example', 'static'])).digest('hex');
const id = '00000000-0000-4000-8000-000000000001';
function client() {
  return { config: { apiUrl: 'http://runtime.example' }, sdk: { getModelAccess: vi.fn().mockResolvedValue({ accesses: [{ kind: 'cloud_byok' }] }) },
    get: vi.fn().mockResolvedValue({ items: [], management_available: true, chatgpt: { available: true } }), post: vi.fn().mockResolvedValue({ model_id: id }),
    put: vi.fn().mockResolvedValue({ model_id: id }), delete: vi.fn().mockResolvedValue({}) };
}
function request(body: unknown, origin = 'http://localhost:3536') {
  return new NextRequest('http://localhost:3536/api/personal-models', { method: 'POST', headers: { origin, 'content-type': 'application/json' }, body: JSON.stringify({ runtime_scope: binding, ...(body as Record<string, unknown>) }) });
}
describe('personal model public routes', () => {
  beforeEach(() => vi.clearAllMocks());
  it('projects disabled ChatGPT without disabling personal API-key management', async () => {
    const runtime = client();
    runtime.get.mockResolvedValue({ items: [], management_available: true, chatgpt: { available: false, detail: 'Disabled by Server' } });
    auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    const response = await GET();
    expect(response.status).toBe(200);
    expect((await response.json()).chatgpt).toEqual({ available: false, detail: 'Disabled by Server' });
    expect(runtime.get).toHaveBeenCalledTimes(2);
  });
  it('returns the canonical effective catalog with the same authenticated settings binding', async () => {
    const runtime = client();
    runtime.sdk.getModelAccess.mockResolvedValue({ offerings: [], accesses: [], default_offering_id: null } as never);
    auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    const response = await GET(new NextRequest(`http://localhost:3536/api/personal-models?catalog=1&owner_id=user&runtime_scope=${binding}`));
    expect(response.status).toBe(200);
    expect((await response.json()).catalog.items).toEqual([]);
    expect(runtime.sdk.getModelAccess).toHaveBeenCalledTimes(1);
  });
  it('rejects an unbound combined catalog before reading model data', async () => {
    const runtime = client(); auth.mockResolvedValue({ user: { user_id: 'different' }, client: runtime, response: null } as never);
    const response = await GET(new NextRequest(`http://localhost:3536/api/personal-models?catalog=1&owner_id=user&runtime_scope=${binding}`));
    expect(response.status).toBe(409); expect(runtime.get).not.toHaveBeenCalled();
    expect(runtime.sdk.getModelAccess).not.toHaveBeenCalled();
  });
  it('pins model mutations to the account that opened settings', async () => {
    const runtime = client();
    auth.mockResolvedValue({ user: { user_id: 'new-user' }, client: runtime, response: null } as never);
    expect((await POST(request({ owner_id: 'former-user', action: 'create', api_key: 'synthetic-key' }))).status).toBe(409);
    expect(runtime.post).not.toHaveBeenCalled();
  });
  it('rejects a changed runtime even when the account ID is unchanged', async () => {
    const runtime = client(); runtime.config.apiUrl = 'http://different-runtime.example';
    auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    expect((await POST(request({ owner_id: 'user', action: 'create', api_key: 'synthetic-key' }))).status).toBe(409);
    expect(runtime.post).not.toHaveBeenCalled();
  });
  it('keeps MOI model access on its existing managed path', async () => {
    const runtime = client(); runtime.get.mockResolvedValue({ items: [], management_available: false });
    auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    expect((await GET()).status).toBe(403);
    expect(runtime.get).toHaveBeenCalledWith('/me/model-connections');
    expect((await POST(request({ owner_id: 'user', action: 'create' }))).status).toBe(403);
    expect(runtime.post).not.toHaveBeenCalled();
  });
  it('does not silently replace a default when checking and adding an API key', async () => {
    const runtime = client(); auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    const result = await POST(request({ owner_id: 'user', action: 'create', name: 'mine', provider: 'openai', model: 'fixture', api_key: 'synthetic-key', context_window: 128000, is_default: true }));
    expect(result.status).toBe(200);
    expect(runtime.post).toHaveBeenCalledWith('/me/models', expect.objectContaining({ api_key: 'synthetic-key', is_default: false }));
    expect(JSON.stringify(await result.json())).not.toContain('synthetic-key');
  });
  it('binds local OAuth to the browser loopback listener', async () => {
    const runtime = client(); auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    expect((await POST(request({ owner_id: 'user', action: 'start' }))).status).toBe(200);
    expect(runtime.post).toHaveBeenCalledWith('/me/model-connections/chatgpt/authorizations', { redirect_uri: 'http://127.0.0.1:3536/auth/callback', connection_id: undefined });
    expect((await POST(request({ owner_id: 'user', action: 'disconnect', connection_id: id }, 'https://other.example'))).status).toBe(403);
  });
  it.each(['localhost', '127.0.0.1'])('rejects a default-port %s start before creating an attempt', async (host) => {
    const runtime = client(); auth.mockResolvedValue({ user: { user_id: 'user' }, client: runtime, response: null } as never);
    const response = await POST(new NextRequest(`http://${host}/api/personal-models`, { method: 'POST', headers: { host, origin: `http://${host}`, 'content-type': 'application/json' }, body: JSON.stringify({ owner_id: 'user', runtime_scope: binding, action: 'start' }) }));
    expect(response.status).toBe(400); expect(runtime.post).not.toHaveBeenCalled();
  });
  it('round trips opaque callback values through the actual script without HTML injection', async () => {
    const values = { state: 'bound<state', code: 'opaque</script>"\\code', client_id: 'client<id' };
    const response = callback(new NextRequest(`http://127.0.0.1:3536/auth/callback?${new URLSearchParams(values)}`));
    const html = await response.text();
    const script = html.match(/<script nonce="[^"]+">([\s\S]*?)<\/script>/)![1];
    const postMessage = vi.fn(); const replaceState = vi.fn();
    runInNewContext(script, { history: { replaceState }, location: { port: '3536' }, window: { opener: { postMessage } } });
    expect(script).not.toContain('</script>');
    expect(postMessage.mock.calls).toEqual([
      [{ type: 'astra-chatgpt-callback', ...values }, 'http://127.0.0.1:3536'],
      [{ type: 'astra-chatgpt-callback', ...values }, 'http://localhost:3536'],
    ]);
    expect(replaceState).toHaveBeenCalledWith(null, '', '/auth/callback');
  });
  it('rejects repeated callback fields and isolates valid callback data from URL history', async () => {
    expect(callback(new NextRequest('http://127.0.0.1:3536/auth/callback?state=a&state=b&code=x')).status).toBe(400);
    const response = callback(new NextRequest('http://127.0.0.1:3536/auth/callback?state=bound&code=%3Cscript%3E&client_id=fixture'));
    const html = await response.text();
    expect(response.headers.get('cache-control')).toBe('no-store');
    expect(response.headers.get('referrer-policy')).toBe('no-referrer');
    expect(html).toContain("history.replaceState(null,'','/auth/callback')");
    expect(html).toContain('http://127.0.0.1'); expect(html).toContain('http://localhost'); expect(html).toContain('location.port');
    expect(html).not.toContain('"code":"<script>"');
  });
});
