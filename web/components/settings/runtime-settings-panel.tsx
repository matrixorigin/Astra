'use client';

import { useEffect, useState, useCallback } from 'react';
import {
  getSessionDevices,
  revokeSessionDevice,
  type DeviceLease,
} from '@/lib/api/session-client';

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
type SettingsTab = 'runtime' | 'personal-skills';

type UserSkillSource = {
  source_id: string;
  skill_name: string;
  visibility: string;
  status: string;
};

type UserSkillVersion = {
  version_id: string;
  skill_name: string;
  version: string;
  content_hash: string;
  normalize_version: string;
  status: string;
  token_estimate: number;
};

export function RuntimeSettingsPanel() {
  const [settingsTab, setSettingsTab] = useState<SettingsTab>('runtime');
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
  const [deviceSessionId, setDeviceSessionId] = useState('');
  const [devices, setDevices] = useState<DeviceLease[]>([]);
  const [personalSkills, setPersonalSkills] = useState<UserSkillSource[]>([]);
  const [skillVersionsByName, setSkillVersionsByName] = useState<Record<string, UserSkillVersion[]>>({});
  const [newSkillName, setNewSkillName] = useState('');
  const [newSkillVersion, setNewSkillVersion] = useState('v1');
  const [newSkillContent, setNewSkillContent] = useState('## Instructions\n\n');
  const [skillSessionId, setSkillSessionId] = useState('');
  const isAuthenticated = Boolean(config?.hasAccessToken && user);

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

  const loadPersonalSkills = useCallback(async () => {
    if (!config?.hasAccessToken) return;
    setBusy(true);
    setStatus(null);
    try {
      const response = await fetch('/api/backend/skills/user', { cache: 'no-store' });
      if (!response.ok) throw new Error(`Failed to load skills (${response.status})`);
      const skills = (await response.json()) as UserSkillSource[];
      const versions: Record<string, UserSkillVersion[]> = {};
      await Promise.all(
        skills.map(async (skill) => {
          const versionResponse = await fetch(
            `/api/backend/skills/user/${encodeURIComponent(skill.skill_name)}/versions`,
            { cache: 'no-store' },
          );
          versions[skill.skill_name] = versionResponse.ok
            ? ((await versionResponse.json()) as UserSkillVersion[])
            : [];
        }),
      );
      setPersonalSkills(skills);
      setSkillVersionsByName(versions);
    } catch (err) {
      setStatus({
        text: err instanceof Error ? err.message : 'Failed to load personal skills.',
        type: 'error',
      });
    } finally {
      setBusy(false);
    }
  }, [config?.hasAccessToken]);

  useEffect(() => {
    if (settingsTab === 'personal-skills' && isAuthenticated) {
      void loadPersonalSkills();
    }
  }, [settingsTab, isAuthenticated, loadPersonalSkills]);

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

  async function loadDevices() {
    if (!deviceSessionId.trim()) {
      setStatus({ text: 'Enter a session id first.', type: 'error' });
      return;
    }
    setBusy(true);
    setStatus(null);
    try {
      setDevices(await getSessionDevices(deviceSessionId.trim()));
      setStatus({ text: 'Loaded session devices.', type: 'success' });
    } catch (err) {
      setStatus({
        text: err instanceof Error ? err.message : 'Failed to load devices.',
        type: 'error',
      });
    } finally {
      setBusy(false);
    }
  }

  async function revokeDevice(device: DeviceLease) {
    setBusy(true);
    setStatus(null);
    try {
      await revokeSessionDevice(device.session_id, {
        leaseId: device.lease_id,
        expectedLastMonotonicId: device.last_monotonic_id,
      });
      setDevices((prev) =>
        prev.map((item) =>
          item.lease_id === device.lease_id ? { ...item, status: 'revoked' } : item,
        ),
      );
      setStatus({ text: 'Device lease revoked.', type: 'success' });
    } catch (err) {
      setStatus({
        text: err instanceof Error ? err.message : 'Failed to revoke device.',
        type: 'error',
      });
    } finally {
      setBusy(false);
    }
  }

  async function submitPersonalSkill() {
    const skillName = newSkillName.trim();
    if (!skillName) {
      setStatus({ text: 'Enter a skill name first.', type: 'error' });
      return;
    }
    setBusy(true);
    setStatus(null);
    try {
      const sourceResponse = await fetch('/api/backend/skills/user', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ skill_name: skillName, visibility: 'private' }),
      });
      if (!sourceResponse.ok) throw new Error(`Create skill failed (${sourceResponse.status})`);
      const versionResponse = await fetch(
        `/api/backend/skills/user/${encodeURIComponent(skillName)}/versions`,
        {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            version: newSkillVersion.trim() || 'v1',
            manifest_json: { name: skillName },
            content_markdown: newSkillContent,
            status: 'draft',
          }),
        },
      );
      if (!versionResponse.ok) throw new Error(`Submit version failed (${versionResponse.status})`);
      setStatus({ text: 'Personal skill version saved.', type: 'success' });
      await loadPersonalSkills();
    } catch (err) {
      setStatus({
        text: err instanceof Error ? err.message : 'Failed to save personal skill.',
        type: 'error',
      });
    } finally {
      setBusy(false);
    }
  }

  async function activateSkill(skillName: string) {
    const sessionId = skillSessionId.trim();
    if (!sessionId) {
      setStatus({ text: 'Enter a session id before activating a skill.', type: 'error' });
      return;
    }
    const versions = skillVersionsByName[skillName] ?? [];
    const version = [...versions].reverse().find((item) => item.status !== 'quarantined');
    if (!version) {
      setStatus({ text: 'No activatable version found.', type: 'error' });
      return;
    }
    setBusy(true);
    setStatus(null);
    try {
      const response = await fetch(
        `/api/backend/skills/user/${encodeURIComponent(skillName)}/activate`,
        {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ session_id: sessionId, version_id: version.version_id }),
        },
      );
      if (!response.ok) throw new Error(`Activate failed (${response.status})`);
      setStatus({ text: `Activated ${skillName}@${version.version}.`, type: 'success' });
    } catch (err) {
      setStatus({
        text: err instanceof Error ? err.message : 'Failed to activate skill.',
        type: 'error',
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <div className="flex gap-2 rounded-2xl border border-slate-800 bg-slate-950/70 p-1">
        {[
          ['runtime', 'Runtime'] as const,
          ['personal-skills', 'Personal Skills'] as const,
        ].map(([key, label]) => (
          <button
            key={key}
            type="button"
            onClick={() => setSettingsTab(key)}
            className={`rounded-xl px-4 py-2 text-sm ${
              settingsTab === key
                ? 'bg-slate-800 text-white'
                : 'text-slate-400 hover:text-slate-200'
            }`}
          >
            {label}
          </button>
        ))}
      </div>

      {settingsTab === 'runtime' ? (
        <>
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
                    Authenticated as {user?.display_name ?? user?.username ?? 'user'}
                  </p>
                  <p className="text-xs text-slate-400">{user?.email ?? ''}</p>
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

      {isAuthenticated ? (
        <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
          <h2 className="text-lg font-semibold text-white">Session Devices</h2>
          <div className="mt-4 flex gap-2">
            <input
              value={deviceSessionId}
              onChange={(event) => setDeviceSessionId(event.target.value)}
              placeholder="Session id"
              className="min-w-0 flex-1 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
            />
            <button
              type="button"
              onClick={loadDevices}
              disabled={busy || !deviceSessionId.trim()}
              className="rounded-2xl border border-slate-700 px-4 py-3 text-sm text-slate-300 hover:border-slate-500 disabled:opacity-50"
            >
              Load
            </button>
          </div>
          <div className="mt-4 space-y-2">
            {devices.map((device) => (
              <div
                key={device.lease_id}
                className="flex items-center justify-between gap-3 rounded-2xl border border-slate-800 bg-slate-950/70 p-3"
              >
                <div className="min-w-0">
                  <p className="truncate text-sm text-white">{device.device_id}</p>
                  <p className="text-xs text-slate-500">
                    {device.trust_level} · {device.status} · expires {device.expires_at}
                  </p>
                </div>
                <button
                  type="button"
                  onClick={() => revokeDevice(device)}
                  disabled={busy || device.status !== 'active'}
                  className="rounded-xl border border-red-800/50 px-3 py-2 text-xs text-red-300 hover:border-red-600 disabled:opacity-40"
                >
                  Revoke
                </button>
              </div>
            ))}
            {devices.length === 0 ? (
              <p className="text-sm text-slate-500">No devices loaded.</p>
            ) : null}
          </div>
        </section>
      ) : null}
        </>
      ) : (
        <section className="rounded-3xl border border-slate-800 bg-slate-900/50 p-6">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
            <div>
              <h2 className="text-lg font-semibold text-white">Personal Skills</h2>
              <p className="mt-1 text-sm text-slate-400">
                Private skill sources and append-only versions stored in MatrixOne.
              </p>
            </div>
            <button
              type="button"
              onClick={loadPersonalSkills}
              disabled={busy || !isAuthenticated}
              className="rounded-2xl border border-slate-700 px-4 py-2 text-sm text-slate-300 hover:border-slate-500 disabled:opacity-50"
            >
              Refresh
            </button>
          </div>

          {!isAuthenticated ? (
            <p className="mt-4 text-sm text-slate-500">Log in to manage personal skills.</p>
          ) : (
            <>
              <div className="mt-5 grid gap-3 lg:grid-cols-[1fr_8rem]">
                <input
                  value={newSkillName}
                  onChange={(event) => setNewSkillName(event.target.value)}
                  placeholder="skill name"
                  className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
                />
                <input
                  value={newSkillVersion}
                  onChange={(event) => setNewSkillVersion(event.target.value)}
                  placeholder="version"
                  className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
                />
              </div>
              <textarea
                value={newSkillContent}
                onChange={(event) => setNewSkillContent(event.target.value)}
                rows={5}
                className="mt-3 w-full rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 font-mono text-sm text-white outline-none"
              />
              <div className="mt-3 flex flex-col gap-3 sm:flex-row">
                <input
                  value={skillSessionId}
                  onChange={(event) => setSkillSessionId(event.target.value)}
                  placeholder="session id for activation"
                  className="min-w-0 flex-1 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
                />
                <button
                  type="button"
                  onClick={submitPersonalSkill}
                  disabled={busy || !newSkillName.trim()}
                  className="rounded-2xl bg-sky-500 px-5 py-3 text-sm font-medium text-slate-950 hover:bg-sky-400 disabled:opacity-50"
                >
                  Save version
                </button>
              </div>

              <div className="mt-6 overflow-hidden rounded-2xl border border-slate-800">
                <table className="min-w-full divide-y divide-slate-800 text-left text-sm">
                  <thead className="bg-slate-950/80 text-xs uppercase text-slate-500">
                    <tr>
                      <th className="px-4 py-3">Skill</th>
                      <th className="px-4 py-3">Versions</th>
                      <th className="px-4 py-3">Latest</th>
                      <th className="px-4 py-3">Action</th>
                    </tr>
                  </thead>
                  <tbody className="divide-y divide-slate-800">
                    {personalSkills.map((skill) => {
                      const versions = skillVersionsByName[skill.skill_name] ?? [];
                      const latest = versions[versions.length - 1];
                      return (
                        <tr key={skill.source_id} className="bg-slate-900/30">
                          <td className="px-4 py-3 text-white">{skill.skill_name}</td>
                          <td className="px-4 py-3 text-slate-300">{versions.length}</td>
                          <td className="px-4 py-3 text-slate-400">
                            {latest
                              ? `${latest.version} · ${latest.status} · ${latest.normalize_version}`
                              : 'none'}
                          </td>
                          <td className="px-4 py-3">
                            <button
                              type="button"
                              onClick={() => activateSkill(skill.skill_name)}
                              disabled={busy || !latest}
                              className="rounded-xl border border-slate-700 px-3 py-2 text-xs text-slate-300 hover:border-slate-500 disabled:opacity-40"
                            >
                              Activate
                            </button>
                          </td>
                        </tr>
                      );
                    })}
                    {personalSkills.length === 0 ? (
                      <tr>
                        <td className="px-4 py-4 text-slate-500" colSpan={4}>
                          No personal skills yet.
                        </td>
                      </tr>
                    ) : null}
                  </tbody>
                </table>
              </div>
            </>
          )}
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
