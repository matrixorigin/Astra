'use client';

import { useState, useEffect, useCallback } from 'react';
import type { EdgeAgent } from '@/lib/api/platform-edges';

const statusColors = {
  connected: '#22c55e',
  stale: '#f59e0b',
} as const;

function formatUptime(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  return m > 0 ? `${h}h ${m}m` : `${h}h`;
}

function edgeStatus(secs: number): 'connected' | 'stale' {
  // If heartbeat is older than 2 min, mark stale
  return secs < 120 ? 'connected' : 'stale';
}

export function EdgesListClient({
  initialEdges,
  isLive,
}: {
  initialEdges: EdgeAgent[];
  isLive: boolean;
}) {
  const [edges, setEdges] = useState(initialEdges);
  const [query, setQuery] = useState('');

  // Auto-refresh every 10s in live mode
  const refresh = useCallback(async () => {
    try {
      const res = await fetch('/api/backend/edges/status');
      if (res.ok) {
        const data = await res.json();
        setEdges(data.edges ?? []);
      }
    } catch {
      // Silently ignore refresh errors
    }
  }, []);

  useEffect(() => {
    if (!isLive) return;
    const interval = setInterval(refresh, 10_000);
    return () => clearInterval(interval);
  }, [isLive, refresh]);

  const filtered = edges.filter((e) => {
    const haystack =
      `${e.edge_agent_id} ${e.hostname ?? ''} ${e.workspace_dir ?? ''}`.toLowerCase();
    return haystack.includes(query.toLowerCase());
  });

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search by hostname, agent ID, workspace…"
          className="flex-1 rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none ring-0 placeholder:text-slate-500"
        />
        {isLive && (
          <button
            onClick={refresh}
            className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-slate-300 transition hover:border-sky-600 hover:text-white"
          >
            Refresh
          </button>
        )}
      </div>

      <div className="flex items-center justify-between">
        <p className="text-xs text-slate-500">
          {filtered.length} edge agent{filtered.length !== 1 ? 's' : ''} connected
        </p>
        {isLive && (
          <p className="text-xs text-slate-500">Auto-refreshing every 10s</p>
        )}
      </div>

      {filtered.length === 0 ? (
        <div className="rounded-2xl border border-dashed border-slate-700 px-4 py-12 text-center">
          <p className="text-sm text-slate-400">
            {edges.length === 0
              ? 'No edge agents connected. Start an astra-edge process to see it here.'
              : 'No edge agents match the current filter.'}
          </p>
          {edges.length === 0 && (
            <div className="mx-auto mt-4 max-w-lg rounded-xl bg-slate-900 p-4 text-left">
              <p className="mb-2 text-xs font-medium text-slate-400">Quick start:</p>
              <code className="block text-xs text-sky-300">
                ASTRA_SERVER_URL=wss://your-server/edge/ws \<br />
                ASTRA_TOKEN=your-jwt-token \<br />
                astra-edge
              </code>
            </div>
          )}
        </div>
      ) : (
        <div className="space-y-4">
          {filtered.map((edge) => {
            const status = edgeStatus(edge.connected_secs);
            const color = statusColors[status];
            return (
              <div
                key={edge.edge_agent_id}
                className="rounded-2xl border border-slate-800 bg-slate-950/70 p-5"
              >
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-3">
                      <h2 className="truncate text-lg font-semibold text-white">
                        {edge.hostname ?? edge.edge_agent_id}
                      </h2>
                      <span
                        className="shrink-0 rounded-full px-3 py-1 text-xs font-medium"
                        style={{
                          backgroundColor: `${color}20`,
                          color,
                          border: `1px solid ${color}40`,
                        }}
                      >
                        {status}
                      </span>
                    </div>
                    <p className="mt-1 text-sm text-slate-400">
                      <span className="font-mono text-xs text-slate-500">
                        {edge.edge_agent_id}
                      </span>
                    </p>
                  </div>
                  <div className="text-right text-sm text-slate-400">
                    <p>Uptime: {formatUptime(edge.connected_secs)}</p>
                  </div>
                </div>

                {edge.workspace_dir && (
                  <div className="mt-3 flex items-center gap-2">
                    <span className="text-xs text-slate-500">Workspace:</span>
                    <code className="rounded bg-slate-800 px-2 py-0.5 text-xs text-slate-300">
                      {edge.workspace_dir}
                    </code>
                  </div>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
