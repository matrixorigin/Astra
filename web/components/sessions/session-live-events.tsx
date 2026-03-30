'use client';

import { useCallback } from 'react';
import { usePolling } from '@/hooks/use-polling';
import { RunStreamViewer } from '@/components/streaming/run-stream-viewer';
import { ConnectionStatus } from '@/components/streaming/connection-status';

type RunListResponse = {
  runs: Array<{
    run_id: string;
    session_id: string;
    status: string;
  }>;
};

export function SessionLiveEvents({
  sessionId,
}: {
  sessionId: string;
}) {
  const fetcher = useCallback(async () => {
    const res = await fetch('/api/backend/runs?limit=5&offset=0', {
      headers: {
        'Content-Type': 'application/json',
      },
      cache: 'no-store',
    });
    if (!res.ok) throw new Error(`Failed to fetch runs: ${res.status}`);
    const data = (await res.json()) as RunListResponse;
    return data.runs
      .filter(
        (r) =>
          r.session_id === sessionId &&
          (r.status === 'running' || r.status === 'waiting'),
      )
      .map((r) => r.run_id);
  }, [sessionId]);

  const { data: activeRunIds, error } = usePolling<string[]>({
    fetcher,
    intervalMs: 10_000,
    enabled: true,
  });

  const activeRunId = activeRunIds?.[0] ?? null;

  return (
    <div className="space-y-4">
      <div className="flex items-center gap-3">
        <h3 className="text-sm font-semibold text-white">Live Run Events</h3>
        {activeRunId ? (
          <ConnectionStatus state="connected" />
        ) : (
          <ConnectionStatus state="disconnected" />
        )}
      </div>

      {error ? (
        <div className="rounded-2xl border border-red-900/50 bg-red-950/30 p-4">
          <p className="text-sm text-red-400">
            Failed to check for active runs: {error}
          </p>
        </div>
      ) : null}

      {activeRunId ? (
        <RunStreamViewer
          runId={activeRunId}
          title={`Live: ${activeRunId}`}
        />
      ) : (
        <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
          <p className="text-sm text-slate-500">
            No active runs for this session. Events will stream here when a run
            starts.
          </p>
        </div>
      )}
    </div>
  );
}
