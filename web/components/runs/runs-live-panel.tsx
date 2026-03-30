'use client';

import { useCallback, useState } from 'react';
import { usePolling } from '@/hooks/use-polling';
import { RunStreamViewer } from '@/components/streaming/run-stream-viewer';
import { ConnectionStatus } from '@/components/streaming/connection-status';
import type { RunSummary } from '@/lib/models/platform';

type RunListResponse = {
  runs: Array<{
    run_id: string;
    session_id: string;
    status: string;
    waiting_for?: string;
    events_count: number;
  }>;
  total: number;
  limit: number;
  offset: number;
};

function normalizeRun(raw: RunListResponse['runs'][number]): RunSummary {
  return {
    runId: raw.run_id,
    sessionId: raw.session_id,
    status: raw.status,
    waitingFor: raw.waiting_for,
    eventsCount: raw.events_count,
  };
}

export function RunsLivePanel() {
  const [enabled, setEnabled] = useState(true);

  const fetcher = useCallback(async () => {
    const res = await fetch('/api/backend/runs?limit=5&offset=0', {
      headers: {
        'Content-Type': 'application/json',
      },
      cache: 'no-store',
    });
    if (!res.ok) throw new Error(`Failed to fetch runs: ${res.status}`);
    const data = (await res.json()) as RunListResponse;
    return data.runs.map(normalizeRun);
  }, []);

  const { data: runs, error } = usePolling<RunSummary[]>({
    fetcher,
    intervalMs: 10_000,
    enabled,
  });

  const activeRuns = (runs ?? []).filter(
    (r) => r.status === 'running' || r.status === 'waiting',
  );
  const newestActive = activeRuns[0] ?? null;

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <h3 className="text-sm font-semibold text-white">Live Activity</h3>
          {newestActive ? (
            <ConnectionStatus state="connected" />
          ) : (
            <ConnectionStatus state="disconnected" />
          )}
        </div>
        <button
          type="button"
          onClick={() => setEnabled((prev) => !prev)}
          className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-300 hover:border-slate-500"
        >
          {enabled ? 'Pause polling' : 'Resume polling'}
        </button>
      </div>

      {error ? (
        <div className="rounded-2xl border border-red-900/50 bg-red-950/30 p-4">
          <p className="text-sm text-red-400">Failed to fetch active runs: {error}</p>
        </div>
      ) : null}

      {newestActive ? (
        <div className="space-y-2">
          <p className="text-xs text-slate-500">
            Streaming events for run{' '}
            <span className="font-mono text-slate-300">{newestActive.runId}</span>{' '}
            ({newestActive.status})
          </p>
          <RunStreamViewer
            runId={newestActive.runId}
            title={`Live: ${newestActive.runId}`}
          />
        </div>
      ) : (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
          <p className="text-sm text-slate-500">
            No active runs detected. Runs with status &quot;running&quot; or
            &quot;waiting&quot; will stream here automatically.
          </p>
        </div>
      )}
    </div>
  );
}
