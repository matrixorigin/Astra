'use client';

import { useEffect, useState, useCallback } from 'react';

type RuntimeConfigResponse = {
  mode: 'live' | 'demo' | 'unconfigured';
  source: 'cookie' | 'env' | 'none';
  apiUrl: string;
  demoMode: boolean;
  hasAccessToken: boolean;
  hasRefreshToken: boolean;
  maskedAccessToken: string | null;
  message: string;
};

type AuthUser = {
  user_id: string;
  username: string;
  email: string;
  display_name?: string;
};

type ConnectionStatus = 'untested' | 'testing' | 'ok' | 'error';

export function RuntimeSettingsPanel() {
  const [config, setConfig] = useState<RuntimeConfigResponse | null>(null);
  const [user, setUser] = useState<AuthUser | null>(null);
  const [apiUrl, setApiUrl] = useState('');
  const [demoMode, setDemoMode] = useState(false);
  const [accessToken, setAccessToken] = useState('');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [status, setStatus] = useState<{ text: string; type: 'success' | 'error' | 'info' } | null>(null);
  const [busy, setBusy] = useState(false);
  const [connStatus, setConnStatus] = useState<ConnectionStatus>('untested');

  const loadConfig = useCallback(async () => {
    const response = await fetch('/api/runtime-config', { cache: 'no-store' });
    const json = (await response.json()) as RuntimeConfigResponse;
    setConfig(json);
    setApiUrl(json.apiUrl ?? '');
    setDemoMode(json.demoMode);
  }, []);

  const loadUser = useCallback(async () => {
    try {
      const meRes = await fetch('/api/runtime-auth/me', { cache: 'no-store' });
      if (meRes.ok) {
        setUser(await meRes.json());
      } else {
        setUser(null);
      }
    } catch {
      setUser(null);
    }
  }, []);

  useEffect(() => {
    void loadConfig();
    void loadUser();
  }, [loadConfig, loadUser]);

  async function testConnection() {
    setConnStatus('testing');
    try {
      const url = apiUrl.replace(/\/$/, '');
      const res = await fetch(`${url}/health`, { signal: AbortSignal.timeout(5000) });
      if (res.ok) {
        setConnStatus('ok');
        setStatus({ text: 'Backend is reachable and healthy.', type: 'success' });
      } else {
        setConnStatus('error');
        setStatus({ text: `Backend returned ${res.status} ${res.statusText}.`, type: 'error' });
      }
    } catch {
      setConnStatus('error');
      setStatus({ text: 'Cannot reach the backend. Check the URL and make sure the server is running.', type: 'error' });
    }
  }

  async function saveConfig() {
    setBusy(true);
    setStatus(null);
    const response = await fetch('/api/runtime-config', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ apiUrl, accessToken: accessToken || undefined, demoMode }),
    });
    setBusy(false);
    if (!response.ok) {
      setStatus({ text: 'Failed to save runtime configuration.', type: 'error' });
      return;
    }
    setAccessToken('');
    setConnStatus('untested');
    setStatus({ text: 'Runtime configuration saved.', type: 'success' });
    await loadConfig();
  }

  async function login() {
    if (!apiUrl) {
      setStatus({ text: 'Set the API base URL first.', type: 'error' });
      return;
    }
    setBusy(true);
    setStatus(null);
    const response = await fetch('/api/runtime-auth/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ apiUrl, username, password }),
    });
    const json = (await response.json()) as { error?: string };
    setBusy(false);
    if (!response.ok) {
      setStatus({ text: json.error ?? 'Login failed.', type: 'error' });
      return;
    }
    setPassword('');
    setStatus({ text: `Logged in as ${username}. Tokens stored.`, type: 'success' });
    await loadConfig();
    await loadUser();
  }

  async function refreshToken() {
    setBusy(true);
    setStatus(null);
    try {
      const res = await fetch('/api/runtime-auth/refresh', { method: 'POST' });
      const data = await res.json();
      if (res.ok) {
        setStatus({ text: 'Access token refreshed successfully.', type: 'success' });
        await loadConfig();
        await loadUser();
      } else {
        setStatus({ text: data.error ?? 'Token refresh failed. Please log in again.', type: 'error' });
      }
    } catch {
      setStatus({ text: 'Token refresh failed. Network error.', type: 'error' });
    }
    setBusy(false);
  }

  async function logout() {
    setBusy(true);
    setStatus(null);
    const response = await fetch('/api/runtime-auth/logout', { method: 'POST' });
    const json = (await response.json()) as { backendLogoutError?: string | null };
    setBusy(false);
    setUser(null);
    setConnStatus('untested');
    setStatus({
      text: json.backendLogoutError
        ? `Logged out. Note: ${json.backendLogoutError}`
        : 'Logged out and cleared all tokens.',
      type: 'success',
    });
    await loadConfig();
  }

  async function clearAll() {
    setBusy(true);
    setStatus(null);
    await fetch('/api/runtime-config', { method: 'DELETE' });
    setBusy(false);
    setApiUrl('');
    setAccessToken('');
    setPassword('');
    setDemoMode(false);
    setUser(null);
    setConnStatus('untested');
    setStatus({ text: 'Cleared all saved configuration and tokens.', type: 'info' });
    await loadConfig();
  }

  const isAuthenticated = config?.hasAccessToken && user;

  return (
    <div className="space-y-6">
      {/* ── Auth status banner ── */}
      <div className={`rounded-2xl border p-4 ${
        isAuthenticated
          ? 'border-green-800/50 bg-green-950/20'
          : config?.hasAccessToken
            ? 'border-yellow-800/50 bg-yellow-950/20'
            : 'border-slate-800 bg-slate-900/50'
      }`}>
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <div className={`h-3 w-3 rounded-full ${
              isAuthenticated ? 'bg-green-500' : config?.hasAccessToken ? 'bg-yellow-500' : 'bg-slate-600'
            }`} />
            <div>
              {isAuthenticated ? (
                <>
                  <p className="text-sm font-medium text-green-300">
                    Authenticated as {user.display_name ?? user.username}
                  </p>
                  <p className="text-xs text-slate-400">{user.email}</p>
                </>
              ) : config?.hasAccessToken ? (
                <p className="text-sm font-medium text-yellow-300">
                  Token saved but user verification failed
                </p>
              ) : (
                <p className="text-sm font-medium text-slate-400">Not authenticated</p>
              )}
            </div>
          </div>
          {isAuthenticated && (
            <div className="flex gap-2">
              <button
                type="button"
                onClick={refreshToken}
                disabled={busy}
                className="rounded-lg border border-slate-700 px-3 py-1.5 text-xs text-slate-300 hover:border-slate-500 disabled:opacity-50"
              >
                Refresh token
              </button>
              <button
                type="button"
                onClick={logout}
                disabled={busy}
                className="rounded-lg border border-red-800/50 px-3 py-1.5 text-xs text-red-400 hover:border-red-600 disabled:opacity-50"
              >
                Logout
              </button>
            </div>
          )}
        </div>
      </div>

      {/* ── Connection state ── */}
      <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold text-white">Connection</h2>
          <div className="flex items-center gap-2">
            {connStatus === 'ok' && <span className="h-2 w-2 rounded-full bg-green-500" />}
            {connStatus === 'error' && <span className="h-2 w-2 rounded-full bg-red-500" />}
            {connStatus === 'testing' && <span className="h-2 w-2 animate-pulse rounded-full bg-yellow-500" />}
            <span className="text-xs text-slate-500 capitalize">{config?.mode ?? 'loading'}</span>
          </div>
        </div>
        <div className="mt-4 grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <InfoCard label="Mode" value={config?.mode ?? 'loading'} />
          <InfoCard label="Source" value={config?.source ?? 'loading'} />
          <InfoCard label="API URL" value={config?.apiUrl || 'not set'} />
          <InfoCard
            label="Access token"
            value={config?.maskedAccessToken ?? (config?.hasAccessToken ? 'saved' : 'not set')}
          />
        </div>
        <p className="mt-4 text-sm text-slate-400">{config?.message ?? 'Loading settings...'}</p>
      </section>

      {/* ── API Configuration ── */}
      <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
        <h2 className="text-lg font-semibold text-white">Runtime API</h2>
        <div className="mt-4 space-y-4">
          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">API base URL</span>
            <div className="flex gap-2">
              <input
                type="url"
                value={apiUrl}
                onChange={(event) => { setApiUrl(event.target.value); setConnStatus('untested'); }}
                placeholder="http://127.0.0.1:8000"
                className="flex-1 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
              />
              <button
                type="button"
                onClick={testConnection}
                disabled={!apiUrl || connStatus === 'testing'}
                className="rounded-2xl border border-slate-700 px-4 py-3 text-sm text-slate-300 hover:border-slate-500 disabled:opacity-50"
              >
                {connStatus === 'testing' ? 'Testing…' : 'Test'}
              </button>
            </div>
          </label>

          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">
              Replace access token <span className="text-slate-600">(optional)</span>
            </span>
            <input
              value={accessToken}
              onChange={(event) => setAccessToken(event.target.value)}
              placeholder="Paste a fresh token to replace the saved one"
              className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
            />
          </label>

          <label className="flex items-center gap-3 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-slate-300">
            <input
              type="checkbox"
              checked={demoMode}
              onChange={(event) => setDemoMode(event.target.checked)}
              className="rounded"
            />
            Enable demo mode <span className="text-xs text-slate-500">(use mock data, no backend needed)</span>
          </label>

          <div className="flex flex-wrap gap-3">
            <button
              type="button"
              onClick={saveConfig}
              disabled={busy}
              className="rounded-full bg-sky-500 px-5 py-2.5 text-sm font-medium text-slate-950 hover:bg-sky-400 disabled:opacity-50"
            >
              Save configuration
            </button>
            <button
              type="button"
              onClick={clearAll}
              disabled={busy}
              className="rounded-full border border-slate-700 px-5 py-2.5 text-sm text-slate-200 hover:border-slate-500 disabled:opacity-50"
            >
              Clear all
            </button>
          </div>
        </div>
      </section>

      {/* ── Login (only show when not authenticated) ── */}
      {!isAuthenticated && (
        <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
          <h2 className="text-lg font-semibold text-white">Authenticate</h2>
          <p className="mt-1 text-sm text-slate-400">
            Log in to the runtime to get access and refresh tokens automatically.
            Or use the <a href="/login" className="text-sky-400 hover:underline">full login page</a>.
          </p>
          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <label className="block">
              <span className="mb-2 block text-sm text-slate-400">Username</span>
              <input
                value={username}
                onChange={(event) => setUsername(event.target.value)}
                autoComplete="username"
                className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
              />
            </label>
            <label className="block">
              <span className="mb-2 block text-sm text-slate-400">Password</span>
              <input
                type="password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                autoComplete="current-password"
                onKeyDown={(e) => { if (e.key === 'Enter') void login(); }}
                className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
              />
            </label>
          </div>
          <div className="mt-4 flex flex-wrap gap-3">
            <button
              type="button"
              onClick={login}
              disabled={busy || !apiUrl || !username || !password}
              className="rounded-full bg-emerald-500 px-5 py-2.5 text-sm font-medium text-slate-950 hover:bg-emerald-400 disabled:opacity-50"
            >
              Login
            </button>
            <a
              href="/register"
              className="rounded-full border border-slate-700 px-5 py-2.5 text-sm text-slate-200 hover:border-slate-500"
            >
              Create account
            </a>
          </div>
        </section>
      )}

      {/* ── Status toast ── */}
      {status ? (
        <div className={`rounded-2xl border px-4 py-3 text-sm ${
          status.type === 'success'
            ? 'border-green-800/50 bg-green-950/20 text-green-300'
            : status.type === 'error'
              ? 'border-red-800/50 bg-red-950/20 text-red-300'
              : 'border-slate-800 bg-slate-950/70 text-slate-300'
        }`}>
          {status.text}
        </div>
      ) : null}
    </div>
  );
}

function InfoCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
      <p className="text-xs uppercase tracking-wide text-slate-500">{label}</p>
      <p className="mt-2 truncate text-sm text-white" title={value}>{value}</p>
    </div>
  );
}
