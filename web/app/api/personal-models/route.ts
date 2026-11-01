import { createHash } from 'node:crypto';
import { jwtSessionHint } from '@/lib/auth/session-cookie';
import { NextRequest, NextResponse } from 'next/server';
import { requireRuntimeUser } from '@/lib/api/auth-guard';
import { RuntimeClientError } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';
function runtimeScope(apiUrl: string | undefined, token: string | undefined) {
  return createHash('sha256').update(JSON.stringify([new URL(apiUrl!).origin, jwtSessionHint(token ?? '') ?? 'static'])).digest('hex');
}
const connections = '/me/model-connections';
const authorizations = `${connections}/chatgpt/authorizations`;
function identifier(value: unknown): string {
  if (typeof value !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(value)) throw new Error('Invalid model or connection ID');
  return value;
}
function failure(error: unknown) {
  return NextResponse.json({ error: error instanceof Error ? error.message : 'Personal model operation failed' },
    { status: error instanceof RuntimeClientError ? error.status : 400, headers: { 'Cache-Control': 'no-store' } });
}
export async function GET() {
  const auth = await requireRuntimeUser();
  if (auth.response) return auth.response;
  try {
    const linked = await auth.client.get<{ items: unknown[]; management_available: boolean }>(connections);
    if (!linked.management_available) return NextResponse.json({ error: 'Your deployment manages model access; personal model configuration is not available.' }, { status: 403 });
    const models = await auth.client.get<{ items: unknown[] }>('/me/models');
    const connectionError = null;
    return NextResponse.json({ owner_id: auth.user.user_id, runtime_scope: runtimeScope(auth.client.config.apiUrl, auth.client.config.accessToken), models: models.items, connections: linked.items, connectionError }, { headers: { 'Cache-Control': 'no-store' } });
  } catch (error) { return failure(error); }
}
export async function POST(request: NextRequest) {
  const auth = await requireRuntimeUser();
  if (auth.response) return auth.response;
  try {
    const incoming = new URL(request.url);
    const expectedOrigin = `${incoming.protocol}//${request.headers.get('host') ?? incoming.host}`;
    if (request.headers.get('origin') !== expectedOrigin) return NextResponse.json({ error: 'Use this Astra page to configure your model.' }, { status: 403 });
    const body = await request.json();
    if (body.owner_id !== auth.user.user_id) return NextResponse.json({ error: 'Account changed; reopen model settings.' }, { status: 409 });
    if (body.runtime_scope !== runtimeScope(auth.client.config.apiUrl, auth.client.config.accessToken)) return NextResponse.json({ error: 'Deployment or session changed; reopen model settings.' }, { status: 409 });
    const capability = await auth.client.get<{ management_available: boolean }>(connections);
    if (!capability.management_available) return NextResponse.json({ error: 'Personal model configuration is not available for this deployment.' }, { status: 403 });
    let result: unknown;
    switch (body.action) {
      case 'create':
        result = await auth.client.post('/me/models', { name: body.name, provider: body.provider, model: body.model,
          api_key: body.api_key ?? '', connection_id: body.connection_id, base_url: body.base_url,
          context_window: body.context_window, is_default: false });
        break;
      case 'default': result = await auth.client.put(`/me/models/${identifier(body.model_id)}`, { is_default: true }); break;
      case 'activate': result = await auth.client.put(`/me/models/${identifier(body.model_id)}`, { is_active: true }); break;
      case 'delete': result = await auth.client.delete(`/me/models/${identifier(body.model_id)}`); break;
      case 'disconnect': result = await auth.client.post(`${connections}/${identifier(body.connection_id)}/disconnect`); break;
      case 'catalog': result = await auth.client.get(`${connections}/${identifier(body.connection_id)}/models`); break;
      case 'start': {
        const origin = new URL(request.headers.get('origin')!);
        if (origin.protocol !== 'http:' || !origin.port || !['localhost', '127.0.0.1'].includes(origin.hostname)) return NextResponse.json({ error: 'ChatGPT browser authorization requires a local HTTP Astra page on a non-default port (for example, 3536). Connect through the TUI on your browser machine for other deployments.' }, { status: 400 });
        const redirect_uri = `http://127.0.0.1${origin.port ? `:${origin.port}` : ''}/auth/callback`;
        result = await auth.client.post(authorizations, { redirect_uri, connection_id: body.connection_id ? identifier(body.connection_id) : undefined });
        break;
      }
      case 'complete': result = await auth.client.post(`${authorizations}/${identifier(body.authorization_id)}/complete`, {
        state: body.state, code: body.code, client_id: body.client_id, error: body.error,
      }); break;
      case 'cancel': result = await auth.client.delete(`${authorizations}/${identifier(body.authorization_id)}`); break;
      default: return NextResponse.json({ error: 'Unknown personal model operation' }, { status: 400 });
    }
    return NextResponse.json(result ?? {}, { headers: { 'Cache-Control': 'no-store' } });
  } catch (error) { return failure(error); }
}
