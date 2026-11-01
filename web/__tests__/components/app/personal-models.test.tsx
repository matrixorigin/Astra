import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { PersonalModels } from '@/components/app/personal-models';
import { requestJson } from '@/lib/api/request';
vi.mock('@/lib/api/request', () => ({ requestJson: vi.fn() }));
const request = vi.mocked(requestJson);
const settings = { owner_id: 'original-user', runtime_scope: 'original-runtime', models: [], connections: [], connectionError: null };
const authorization = '00000000-0000-4000-8000-000000000001';
function popup() {
  return { closed: false, close: vi.fn(), location: { replace: vi.fn() } } as unknown as Window;
}
function started() {
  const url = new URL('https://auth.openai.com/api/accounts/authorize');
  url.searchParams.set('state', 'bound-state');
  url.searchParams.set('redirect_uri', `http://127.0.0.1${location.port ? `:${location.port}` : ''}/auth/callback`);
  return { authorization_id: authorization, state: 'bound-state', authorization_url: url.toString() };
}
async function ready() { await screen.findByRole('button', { name: 'Continue with ChatGPT' }); }
function actions() {
  return request.mock.calls.filter(([, init]) => init?.method === 'POST').map(([, init]) => JSON.parse(init?.body as string));
}
describe('personal source setup', () => {
  beforeEach(() => { vi.clearAllMocks(); request.mockResolvedValue(settings as never); });
  afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
  it('explains the non-default port requirement before opening a popup', async () => {
    vi.stubGlobal('location', new URL('http://localhost'));
    const open = vi.spyOn(window, 'open').mockReturnValue(popup());
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    await screen.findByText(/with a non-default port/);
    expect(open).not.toHaveBeenCalled(); expect(actions()).toEqual([]);
  });
  it('does not create a server attempt when the popup is blocked', async () => {
    vi.spyOn(window, 'open').mockReturnValue(null);
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    expect(await screen.findByText('Allow the authorization popup, then try again.')).toBeInTheDocument();
    expect(actions()).toEqual([]);
  });
  it('accepts only the bound popup origin and state and completes each code once', async () => {
    const browser = popup(); vi.spyOn(window, 'open').mockReturnValue(browser);
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return { ...settings, connections: [{ connection_id: authorization, label: 'Existing', status: 'connected' }] } as never;
      const body = JSON.parse(init.body as string);
      if (body.action === 'start') return started() as never;
      if (body.action === 'complete') return { connection: { connection_id: authorization, label: 'Personal', status: 'connected' } } as never;
      if (body.action === 'catalog') return { items: [{ slug: 'fixture', display_name: 'Fixture' }], configured_models: [] } as never;
      return {} as never;
    });
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Choose models' }));
    await screen.findByRole('combobox', { name: 'ChatGPT model' });
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    expect(screen.queryByRole('combobox', { name: 'ChatGPT model' })).toBeNull();
    await waitFor(() => expect(browser.location.replace).toHaveBeenCalled());
    const data = { type: 'astra-chatgpt-callback', state: 'bound-state', code: 'synthetic-code', client_id: 'fixture' };
    await act(async () => window.dispatchEvent(new MessageEvent('message', { source: browser, origin: 'https://other.example', data })));
    expect(actions().filter((row) => row.action === 'complete')).toHaveLength(0);
    const origin = `http://127.0.0.1${location.port ? `:${location.port}` : ''}`;
    await act(async () => window.dispatchEvent(new MessageEvent('message', { source: browser, origin, data: { ...data, state: 'wrong' } })));
    expect(actions().filter((row) => row.action === 'complete')).toHaveLength(0);
    await act(async () => {
      window.dispatchEvent(new MessageEvent('message', { source: browser, origin, data }));
      window.dispatchEvent(new MessageEvent('message', { source: browser, origin, data }));
    });
    await screen.findByText('Account connected. Choose and check a model before chatting.');
    expect(actions().filter((row) => row.action === 'complete')).toEqual([expect.objectContaining({ owner_id: settings.owner_id, runtime_scope: settings.runtime_scope, authorization_id: authorization, code: 'synthetic-code' })]);
  });
  it('cancels a pending attempt without completing it or losing the account binding', async () => {
    const browser = popup(); vi.spyOn(window, 'open').mockReturnValue(browser);
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return settings as never;
      return (JSON.parse(init.body as string).action === 'start' ? started() : {}) as never;
    });
    const panel = render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    await waitFor(() => expect(browser.location.replace).toHaveBeenCalled());
    panel.unmount();
    await waitFor(() => expect(actions().filter((row) => row.action === 'cancel')).toEqual([expect.objectContaining({ authorization_id: authorization, owner_id: settings.owner_id, runtime_scope: settings.runtime_scope })]));
    expect(actions().some((row) => row.action === 'complete')).toBe(false);
  });
  it('cancels an authorization that starts after the panel was unmounted', async () => {
    const browser = popup(); vi.spyOn(window, 'open').mockReturnValue(browser);
    let finish!: (value: unknown) => void;
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return settings as never;
      if (JSON.parse(init.body as string).action === 'start') return new Promise((resolve) => { finish = resolve; }) as never;
      return {} as never;
    });
    const panel = render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    await waitFor(() => expect(finish).toBeDefined());
    panel.unmount();
    await act(async () => finish(started()));
    await waitFor(() => expect(actions().filter((row) => row.action === 'cancel')).toEqual([expect.objectContaining({ authorization_id: authorization, owner_id: settings.owner_id, runtime_scope: settings.runtime_scope })]));
    expect(browser.location.replace).not.toHaveBeenCalled();
    expect(actions().some((row) => row.action === 'complete')).toBe(false);
  });
  it.each(['owner_id', 'runtime_scope'] as const)('does not adopt a different %s during a post-mutation refresh', async (binding) => {
    let reads = 0;
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return (++reads === 1 ? settings : { ...settings, [binding]: 'different-binding' }) as never;
      return { model_id: authorization } as never;
    });
    const changed = vi.fn(); render(<PersonalModels onChanged={changed} />); await ready();
    fireEvent.change(screen.getByLabelText('Model name'), { target: { value: 'mine' } });
    fireEvent.change(screen.getByLabelText('Provider model ID'), { target: { value: 'fixture' } });
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'synthetic-key' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check and add personal model' }));
    await screen.findByText(/Account or deployment changed; close and reopen model settings/);
    expect(changed).not.toHaveBeenCalledWith(authorization, expect.anything());
    expect(actions()[0]).toMatchObject({ action: 'create', owner_id: settings.owner_id, runtime_scope: settings.runtime_scope });
  });
  it.each(['failed', 'stalled', 'late'])('retains the committed connection when settings %s and retries models without authorization', async (read) => {
    const browser = popup(); vi.spyOn(window, 'open').mockReturnValue(browser);
    let connected = false;
    let finishRead!: (value: unknown) => void;
    request.mockImplementation(async (_path, init) => {
      if (!init?.method && connected) {
        if (read === 'failed') throw new Error('Settings temporarily unavailable');
        if (read === 'late') return new Promise((resolve) => { finishRead = resolve; }) as never;
        return new Promise(() => {}) as never;
      }
      if (!init?.method) return { ...settings, connections: connected ? [{ connection_id: authorization, email: 'account@example.test', label: 'Personal', status: 'connected' }] : [] } as never;
      const body = JSON.parse(init.body as string);
      if (body.action === 'start') return started() as never;
      if (body.action === 'complete') { connected = true; return { connection: { connection_id: authorization, email: 'account@example.test', label: 'Personal', status: 'connected' } } as never; }
      if (body.action === 'catalog') throw new Error('Catalog temporarily unavailable');
      return {} as never;
    });
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    await waitFor(() => expect(browser.location.replace).toHaveBeenCalled());
    const origin = `http://127.0.0.1${location.port ? `:${location.port}` : ''}`;
    await act(async () => window.dispatchEvent(new MessageEvent('message', { source: browser, origin, data: { type: 'astra-chatgpt-callback', state: 'bound-state', code: 'synthetic-code', client_id: 'fixture' } })));
    await screen.findByText(/temporarily unavailable/);
    expect(connected).toBe(true);
    if (read === 'late') {
      await act(async () => finishRead({ ...settings, connections: [{ connection_id: authorization, email: 'account@example.test', status: 'connected' }], catalog: { items: [], accesses: [], defaultOfferingId: null } }));
      expect(screen.getByRole('status')).toHaveTextContent(/Could not load models/);
    }
    // The authoritative connection is already committed. The view should
    // retain it and allow a read-only catalog retry without another consent.
    expect(screen.queryByText(/account@example.test/)).not.toBeNull();
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return settings as never;
      return { items: [{ slug: 'fixture', display_name: 'Fixture' }], configured_models: [] } as never;
    });
    fireEvent.click(screen.getByRole('button', { name: 'Choose models' }));
    await screen.findByRole('combobox', { name: 'ChatGPT model' });
    expect(actions().filter((row) => row.action === 'complete')).toHaveLength(1);
    expect(actions().filter((row) => row.action === 'start')).toHaveLength(1);
  });

  it('serializes a settings refresh before a new browser authorization', async () => {
    let reads = 0;
    let finish!: (value: unknown) => void;
    request.mockImplementation(async () => {
      if (++reads === 1) return settings as never;
      return new Promise((resolve) => { finish = resolve; }) as never;
    });
    const open = vi.spyOn(window, 'open').mockReturnValue(popup());
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Refresh model sources' }));
    expect(screen.getByRole('button', { name: 'Continue with ChatGPT' })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    expect(open).not.toHaveBeenCalled();
    await act(async () => finish(settings));
    expect(screen.getByRole('button', { name: 'Continue with ChatGPT' })).toBeEnabled();
  });
  it('ignores an initial snapshot superseded by refresh and successful authorization', async () => {
    const browser = popup(); vi.spyOn(window, 'open').mockReturnValue(browser);
    let reads = 0;
    let initial!: (value: unknown) => void;
    const connection = { connection_id: authorization, email: 'current@example.test', label: 'Personal', status: 'connected' };
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) {
        if (++reads === 1) return new Promise((resolve) => { initial = resolve; }) as never;
        return (reads === 2 ? settings : { ...settings, connections: [connection] }) as never;
      }
      const body = JSON.parse(init.body as string);
      if (body.action === 'start') return started() as never;
      if (body.action === 'complete') return { connection } as never;
      return { items: [], configured_models: [] } as never;
    });
    render(<PersonalModels onChanged={vi.fn()} />);
    await waitFor(() => expect(initial).toBeDefined());
    fireEvent.click(screen.getByRole('button', { name: 'Refresh model sources' })); await ready();
    await waitFor(() => expect(screen.getByRole('button', { name: 'Continue with ChatGPT' })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
    await waitFor(() => expect(browser.location.replace).toHaveBeenCalled());
    await act(async () => window.dispatchEvent(new MessageEvent('message', { source: browser,
      origin: `http://127.0.0.1${location.port ? `:${location.port}` : ''}`,
      data: { type: 'astra-chatgpt-callback', state: 'bound-state', code: 'synthetic-code', client_id: 'fixture' } })));
    await screen.findByText(/current@example.test/);
    await act(async () => initial(settings));
    expect(screen.queryByText(/current@example.test/)).not.toBeNull();
    expect(screen.getByRole('button', { name: 'Choose models' })).toBeEnabled();
  });
  it('keeps a confirmed write when refreshing sources fails and retries only the read', async () => {
    let saved = false; let retry = false;
    const catalog = { items: [], accesses: [], defaultOfferingId: null };
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) { if (saved && !retry) throw new Error('offline'); return { ...settings, catalog } as never; }
      saved = true; return { model_id: authorization } as never;
    });
    const changed = vi.fn(); render(<PersonalModels onChanged={changed} />); await ready();
    fireEvent.change(screen.getByLabelText('Model name'), { target: { value: 'mine' } });
    fireEvent.change(screen.getByLabelText('Provider model ID'), { target: { value: 'fixture' } });
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'synthetic-key' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check and add personal model' }));
    await screen.findByText(/Model change saved. Could not refresh/);
    expect(changed).toHaveBeenCalledWith(); retry = true;
    fireEvent.click(screen.getByRole('button', { name: 'Refresh model sources' }));
    await waitFor(() => expect(changed).toHaveBeenCalledWith(authorization, catalog));
    expect(actions().filter((row) => row.action === 'create')).toHaveLength(1);
  });
  it('hands a late committed write to the parent after settings unmount', async () => {
    let finish!: (value: unknown) => void;
    request.mockImplementation(async (_path, init) => {
      if (!init?.method) return settings as never;
      return new Promise((resolve) => { finish = resolve; }) as never;
    });
    const changed = vi.fn(); const panel = render(<PersonalModels onChanged={changed} />); await ready();
    fireEvent.change(screen.getByLabelText('Model name'), { target: { value: 'mine' } });
    fireEvent.change(screen.getByLabelText('Provider model ID'), { target: { value: 'fixture' } });
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'synthetic-key' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check and add personal model' }));
    await waitFor(() => expect(finish).toBeDefined()); panel.unmount();
    await act(async () => finish({ model_id: authorization }));
    expect(changed).toHaveBeenCalledWith(undefined, null);
  });
});
