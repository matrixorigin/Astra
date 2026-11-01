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
      if (!init) return settings as never;
      const body = JSON.parse(init.body as string);
      if (body.action === 'start') return started() as never;
      if (body.action === 'complete') return { connection: { connection_id: authorization } } as never;
      if (body.action === 'catalog') return { items: [{ slug: 'fixture', display_name: 'Fixture' }], configured_models: [] } as never;
      return {} as never;
    });
    render(<PersonalModels onChanged={vi.fn()} />); await ready();
    fireEvent.click(screen.getByRole('button', { name: 'Continue with ChatGPT' }));
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
      if (!init) return settings as never;
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
      if (!init) return settings as never;
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
      if (!init) return (++reads === 1 ? settings : { ...settings, [binding]: 'different-binding' }) as never;
      return { model_id: authorization } as never;
    });
    const changed = vi.fn(); render(<PersonalModels onChanged={changed} />); await ready();
    fireEvent.change(screen.getByLabelText('Model name'), { target: { value: 'mine' } });
    fireEvent.change(screen.getByLabelText('Provider model ID'), { target: { value: 'fixture' } });
    fireEvent.change(screen.getByLabelText('API key'), { target: { value: 'synthetic-key' } });
    fireEvent.click(screen.getByRole('button', { name: 'Check and add personal model' }));
    await screen.findByText('Account or deployment changed; close and reopen model settings.');
    expect(changed).not.toHaveBeenCalled();
    expect(actions()[0]).toMatchObject({ action: 'create', owner_id: settings.owner_id, runtime_scope: settings.runtime_scope });
  });
});
