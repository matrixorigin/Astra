'use client';

import { useEffect, useRef, useState } from 'react';
import { requestJson } from '@/lib/api/request';

type Connection = { connection_id: string; label: string; email?: string; status: string };
type PersonalModel = { model_id: string; name: string; provider: string; is_active: boolean; is_default: boolean };
type Settings = { owner_id: string; runtime_scope: string; models: PersonalModel[]; connections: Connection[] };
type Started = { authorization_id: string; state: string; authorization_url: string };
type Catalog = { items: { slug: string; display_name: string }[]; configured_models: { model_id: string; name: string }[] };
type Pending = { popup: Window; owner: string; scope: string; id?: string; completing: boolean; cancelled: boolean; stop: () => void };

export function PersonalModels({ onChanged }: { onChanged: (modelId?: string) => void }) {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [message, setMessage] = useState('Loading your model sources…');
  const [busy, setBusy] = useState(false);
  const [provider, setProvider] = useState('openai');
  const [catalog, setCatalog] = useState<{ connection: string; models: Catalog } | null>(null);
  const pending = useRef<Pending | null>(null);
  const mounted = useRef(true);
  const readVersion = useRef(0);

  const load = async () => {
    const version = ++readVersion.current;
    const value = await requestJson<Settings>('/api/personal-models');
    if (version !== readVersion.current) return;
    if (settings && (settings.owner_id !== value.owner_id || settings.runtime_scope !== value.runtime_scope)) throw new Error('Account or deployment changed; close and reopen model settings.');
    if (mounted.current) setSettings(value);
  };
  useEffect(() => {
    mounted.current = true;
    let active = true;
    const version = ++readVersion.current;
    void requestJson<Settings>('/api/personal-models').then((value) => {
      if (active && version === readVersion.current) { setSettings(value); setMessage('Credentials belong to you and are stored encrypted on this Astra deployment.'); }
    }).catch((error: Error) => { if (active && version === readVersion.current) setMessage(error.message); });
    return () => { active = false; mounted.current = false; cancelPending(); };
    // The panel pins its account until closed; mutations reject account changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function call<T>(action: string, body: Record<string, unknown> = {}, owner = settings?.owner_id, scope = settings?.runtime_scope) {
    return requestJson<T>('/api/personal-models', { method: 'POST', body: JSON.stringify({ ...body, action, owner_id: owner, runtime_scope: scope }) });
  }
  function cancelPending() {
    const current = pending.current;
    if (!current || current.completing) return;
    current.cancelled = true;
    current.stop();
    current.popup.close();
    if (current.id) void call('cancel', { authorization_id: current.id }, current.owner, current.scope).catch(() => {});
    pending.current = null;
    if (mounted.current) { setBusy(false); setMessage('ChatGPT authorization cancelled. Your conversation is unchanged.'); }
  }
  async function mutation(action: string, body: Record<string, unknown>, select = false) {
    setBusy(true);
    try {
      const result = await call<{ model_id?: string }>(action, body);
      await load();
      if (mounted.current) {
        setMessage(select ? 'Model checked. You can select it in the model menu.' : 'Model sources updated. Your conversation is unchanged.');
        onChanged(select ? result.model_id : undefined);
      }
    } catch (error) { if (mounted.current) setMessage(error instanceof Error ? error.message : 'Model operation failed'); }
    finally { if (mounted.current) setBusy(false); }
  }
  async function chooseModels(connection: string) {
    setBusy(true);
    setCatalog(null);
    try {
      const models = await call<Catalog>('catalog', { connection_id: connection });
      if (mounted.current) {
        setCatalog({ connection, models });
        setMessage(models.items.length ? 'Account connected. Choose and check a model before chatting.' : 'Account connected, but no usable models are available. Choose another source.');
      }
    } catch (error) { if (mounted.current) setMessage(error instanceof Error ? error.message : 'Could not load models. Choose models to retry.'); }
    finally { if (mounted.current) setBusy(false); }
  }
  async function connect(connection?: string) {
    if (!settings || busy) return;
    if (location.protocol !== 'http:' || !location.port || !['localhost', '127.0.0.1'].includes(location.hostname)) {
      setMessage('For browser authorization, open this local Astra installation using HTTP on localhost or 127.0.0.1 with a non-default port (for example, 3536). Use the TUI on your browser machine for other deployments.');
      return;
    }
    const popup = window.open('about:blank', `astra-chatgpt-${crypto.randomUUID()}`, 'popup,width=600,height=760');
    if (!popup) { setMessage('Allow the authorization popup, then try again.'); return; }
    const current: Pending = { popup, owner: settings.owner_id, scope: settings.runtime_scope, completing: false, cancelled: false, stop: () => {} };
    pending.current = current;
    setCatalog(null);
    setBusy(true);
    setMessage('Waiting for ChatGPT consent in your browser…');
    try {
      const started = await call<Started>('start', { connection_id: connection }, current.owner, current.scope);
      current.id = started.authorization_id;
      if (current.cancelled || !mounted.current) {
        await call('cancel', { authorization_id: current.id }, current.owner, current.scope);
        return;
      }
      const redirect = `http://127.0.0.1${location.port ? `:${location.port}` : ''}/auth/callback`;
      const url = new URL(started.authorization_url);
      if (url.origin !== 'https://auth.openai.com' || url.pathname !== '/api/accounts/authorize'
          || url.searchParams.getAll('state').length !== 1 || url.searchParams.get('state') !== started.state
          || url.searchParams.getAll('redirect_uri').length !== 1 || url.searchParams.get('redirect_uri') !== redirect) throw new Error('Invalid authorization destination');
      const receive = (event: MessageEvent) => {
        const value = event.data;
        if (event.source !== popup || event.origin !== new URL(redirect).origin || !value || typeof value !== 'object'
            || value.type !== 'astra-chatgpt-callback' || value.state !== started.state
            || Boolean(value.code) === Boolean(value.error) || current.completing || current.cancelled) return;
        current.completing = true;
        current.stop();
        popup.close();
        setMessage('Completing authorization…');
        void call<{ connection: Connection }>('complete', {
          authorization_id: current.id, state: started.state,
          code: value.code, client_id: value.client_id, error: value.error,
        }, current.owner, current.scope).then(async (linked) => {
          if (!mounted.current) return;
          setSettings((previous) => previous ? { ...previous, connections: [...previous.connections.filter((row) => row.connection_id !== linked.connection.connection_id), linked.connection] } : previous);
          void load().catch((error: Error) => { if (mounted.current) setMessage(error.message); });
          if (linked.connection.status === 'connected') await chooseModels(linked.connection.connection_id);
          else if (mounted.current) setMessage(`Account authorization saved: ${linked.connection.status}. Reconnect to grant plan access or choose another source.`);
        }).catch((error: Error) => { if (mounted.current) setMessage(error.message); })
          .finally(() => { pending.current = null; if (mounted.current) setBusy(false); });
      };
      const timer = window.setInterval(() => { if (popup.closed && !current.completing) cancelPending(); }, 500);
      const timeout = window.setTimeout(cancelPending, 900_000);
      window.addEventListener('message', receive);
      current.stop = () => { window.clearInterval(timer); window.clearTimeout(timeout); window.removeEventListener('message', receive); };
      popup.location.replace(url.toString());
    } catch (error) {
      cancelPending();
      if (mounted.current) setMessage(error instanceof Error ? error.message : 'Authorization failed');
    }
  }

  return <section aria-label="Personal model sources" className="space-y-3 border-t border-border p-3 text-sm">
    <p role="status" className="text-text-muted">{message}</p>
    <button type="button" disabled={busy} onClick={() => { setBusy(true); void load().catch((error: Error) => setMessage(error.message)).finally(() => setBusy(false)); }}>Refresh model sources</button>
    {settings ? <>
      <form className="space-y-2" onSubmit={(event) => {
        event.preventDefault();
        const form = event.currentTarget;
        const data = new FormData(form);
        void mutation('create', { provider, name: data.get('name'), model: data.get('model'), api_key: data.get('api_key'),
          context_window: Number(data.get('context_window')), base_url: provider === 'openai-compatible' ? data.get('base_url') : undefined }, true);
        (form.elements.namedItem('api_key') as HTMLInputElement).value = '';
      }}>
        <h3 className="font-medium">Use my API key</h3>
        <select aria-label="API provider" value={provider} onChange={(event) => setProvider(event.target.value)} disabled={busy}>
          {['openai', 'anthropic', 'deepseek', 'openai-compatible'].map((value) => <option key={value}>{value}</option>)}
        </select>
        <input aria-label="Model name" name="name" placeholder="Name in Astra" required disabled={busy} />
        <input aria-label="Provider model ID" name="model" placeholder="Provider model ID" required disabled={busy} />
        <input aria-label="API key" name="api_key" type="password" autoComplete="off" placeholder="API key" required disabled={busy} />
        <input aria-label="Context tokens" name="context_window" type="number" min="1" defaultValue="128000" required disabled={busy} />
        {provider === 'openai-compatible' ? <input aria-label="API base URL" name="base_url" type="url" placeholder="HTTPS API base URL" required disabled={busy} /> : null}
        <button type="submit" disabled={busy}>Check and add personal model</button>
      </form>
      <div className="space-y-2">
        <h3 className="font-medium">My ChatGPT accounts</h3>
        <button type="button" disabled={busy} onClick={() => void connect()}>Continue with ChatGPT</button>
        {busy && pending.current && !pending.current.completing ? <button type="button" onClick={cancelPending}>Cancel authorization</button> : null}
        {settings.connections.map((connection) => <div key={connection.connection_id}>
          <span>{connection.email || connection.label} · {connection.status} · Your ChatGPT plan</span>
          <button type="button" disabled={busy || connection.status !== 'connected'} onClick={() => void chooseModels(connection.connection_id)}>Choose models</button>
          <button type="button" disabled={busy} onClick={() => { setCatalog(null); void connect(connection.connection_id); }}>Reconnect</button>
          <button type="button" disabled={busy} onClick={() => { setCatalog(null); void mutation('disconnect', { connection_id: connection.connection_id }); }}>Disconnect</button>
        </div>)}
      </div>
      {catalog ? <div className="space-y-2">
        {catalog.models.configured_models.map((model) => <button key={model.model_id} type="button" disabled={busy}
          onClick={() => void mutation('activate', { model_id: model.model_id }, true)}>Check and restore {model.name}</button>)}
        {catalog.models.items.length ? <form onSubmit={(event) => {
          event.preventDefault(); const data = new FormData(event.currentTarget);
          void mutation('create', { provider: 'chatgpt', connection_id: catalog.connection, name: data.get('name'), model: data.get('model'), context_window: Number(data.get('context_window')) }, true);
        }}>
          <select aria-label="ChatGPT model" name="model" required disabled={busy}>{catalog.models.items.map((model) => <option key={model.slug} value={model.slug}>{model.display_name}</option>)}</select>
          <input aria-label="ChatGPT model name" name="name" placeholder="Name in Astra" required disabled={busy} />
          <input aria-label="ChatGPT context tokens" name="context_window" type="number" min="1" defaultValue="128000" required disabled={busy} />
          <button type="submit" disabled={busy}>Check and add ChatGPT model</button>
        </form> : null}
      </div> : null}
      <div><h3 className="font-medium">My personal models</h3>{settings.models.map((model) => <div key={model.model_id}>
        <span>{model.name} · {model.provider} · {model.is_active ? 'Enabled' : 'Inactive'}</span>
        <button type="button" disabled={busy || !model.is_active || model.is_default} onClick={() => void mutation('default', { model_id: model.model_id })}>{model.is_default ? 'Default' : 'Make default'}</button>
        <button type="button" disabled={busy} onClick={() => { if (window.confirm(`Remove your model ${model.name}? Conversation history is retained.`)) void mutation('delete', { model_id: model.model_id }); }}>Remove</button>
      </div>)}</div>
    </> : null}
  </section>;
}
