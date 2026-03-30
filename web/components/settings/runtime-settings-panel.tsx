'use client';

import { useEffect, useState } from 'react';

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

export function RuntimeSettingsPanel() {
  const [config, setConfig] = useState<RuntimeConfigResponse | null>(null);
  const [apiUrl, setApiUrl] = useState('');
  const [demoMode, setDemoMode] = useState(false);
  const [accessToken, setAccessToken] = useState('');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function loadConfig() {
    const response = await fetch('/api/runtime-config', { cache: 'no-store' });
    const json = (await response.json()) as RuntimeConfigResponse;
    setConfig(json);
    setApiUrl(json.apiUrl ?? '');
    setDemoMode(json.demoMode);
  }

  useEffect(() => {
    void loadConfig();
  }, []);

  async function saveConfig() {
    setBusy(true);
    setStatus(null);

    const response = await fetch('/api/runtime-config', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        apiUrl,
        accessToken: accessToken || undefined,
        demoMode,
      }),
    });

    setBusy(false);

    if (!response.ok) {
      setStatus('Failed to save runtime configuration.');
      return;
    }

    setAccessToken('');
    setStatus('Runtime configuration saved.');
    await loadConfig();
  }

  async function login() {
    setBusy(true);
    setStatus(null);

    const response = await fetch('/api/runtime-auth/login', {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
      },
      body: JSON.stringify({
        apiUrl,
        username,
        password,
      }),
    });

    const json = (await response.json()) as { error?: string };
    setBusy(false);

    if (!response.ok) {
      setStatus(json.error ?? 'Login failed.');
      return;
    }

    setPassword('');
    setStatus('Logged in and stored runtime tokens.');
    await loadConfig();
  }

  async function logout() {
    setBusy(true);
    setStatus(null);
    const response = await fetch('/api/runtime-auth/logout', { method: 'POST' });
    const json = (await response.json()) as { backendLogoutError?: string | null };
    setBusy(false);
    setStatus(
      json.backendLogoutError
        ? `Cleared saved runtime credentials. ${json.backendLogoutError}`
        : 'Cleared saved runtime credentials.',
    );
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
    setStatus('Cleared saved runtime config and tokens.');
    await loadConfig();
  }

  return (
    <div className="space-y-6">
      <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
        <h2 className="text-xl font-semibold text-white">Current connection state</h2>
        <div className="mt-4 grid gap-4 md:grid-cols-2 xl:grid-cols-4">
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

      <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
        <h2 className="text-xl font-semibold text-white">Runtime API configuration</h2>
        <div className="mt-4 space-y-4">
          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">API base URL</span>
            <input
              value={apiUrl}
              onChange={(event) => setApiUrl(event.target.value)}
              placeholder="http://127.0.0.1:8000"
              className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
            />
          </label>

          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">
              Replace access token (optional)
            </span>
            <input
              value={accessToken}
              onChange={(event) => setAccessToken(event.target.value)}
              placeholder="Paste a fresh access token to replace the saved one"
              className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
            />
          </label>

          <label className="flex items-center gap-3 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-slate-300">
            <input
              type="checkbox"
              checked={demoMode}
              onChange={(event) => setDemoMode(event.target.checked)}
            />
            Enable demo mode
          </label>

          <div className="flex flex-wrap gap-3">
            <button
              type="button"
              onClick={saveConfig}
              disabled={busy}
              className="rounded-full bg-sky-500 px-4 py-2 text-sm font-medium text-slate-950 disabled:opacity-50"
            >
              Save configuration
            </button>
            <button
              type="button"
              onClick={clearAll}
              disabled={busy}
              className="rounded-full border border-slate-700 px-4 py-2 text-sm text-slate-200 disabled:opacity-50"
            >
              Clear saved config
            </button>
          </div>
        </div>
      </section>

      <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
        <h2 className="text-xl font-semibold text-white">Login to runtime</h2>
        <div className="mt-4 grid gap-4 md:grid-cols-2">
          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">Username</span>
            <input
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
            />
          </label>

          <label className="block">
            <span className="mb-2 block text-sm text-slate-400">Password</span>
            <input
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              className="w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
            />
          </label>
        </div>

        <div className="mt-4 flex flex-wrap gap-3">
          <button
            type="button"
            onClick={login}
            disabled={busy || !apiUrl}
            className="rounded-full bg-emerald-500 px-4 py-2 text-sm font-medium text-slate-950 disabled:opacity-50"
          >
            Login and store tokens
          </button>
          <button
            type="button"
            onClick={logout}
            disabled={busy}
            className="rounded-full border border-slate-700 px-4 py-2 text-sm text-slate-200 disabled:opacity-50"
          >
            Logout and clear tokens
          </button>
        </div>
      </section>

      {status ? (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-slate-300">
          {status}
        </div>
      ) : null}
    </div>
  );
}

function InfoCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
      <p className="text-xs uppercase tracking-wide text-slate-500">{label}</p>
      <p className="mt-2 text-sm text-white">{value}</p>
    </div>
  );
}
