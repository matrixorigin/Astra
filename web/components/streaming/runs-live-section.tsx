'use client';

import { useState } from 'react';
import { RunStreamViewer } from '@/components/streaming/run-stream-viewer';
import type { RunSummary } from '@/lib/models/platform';

export function RunsLiveSection({
  runs,
}: {
  runs: RunSummary[];
}) {
  const activeRuns = runs.filter((r) => r.status === 'running' || r.status === 'waiting');
  const [selectedRunId, setSelectedRunId] = useState<string | null>(
    activeRuns[0]?.runId ?? null,
  );

  if (activeRuns.length === 0) {
    return (
      <div className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
        <p className="text-sm text-slate-500">No active runs to stream. Runs with status &quot;running&quot; or &quot;waiting&quot; will appear here.</p>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {activeRuns.length > 1 ? (
        <div className="flex flex-wrap gap-2">
          {activeRuns.map((run) => (
            <button
              key={run.runId}
              type="button"
              onClick={() => setSelectedRunId(run.runId)}
              className={`rounded-full border px-3 py-1 text-xs ${
                selectedRunId === run.runId
                  ? 'border-sky-500 bg-sky-500/10 text-sky-300'
                  : 'border-slate-700 text-slate-300 hover:border-slate-500'
              }`}
            >
              {run.runId} ({run.status})
            </button>
          ))}
        </div>
      ) : null}

      {selectedRunId ? (
        <RunStreamViewer
          runId={selectedRunId}
          title={`Streaming: ${selectedRunId}`}
        />
      ) : null}
    </div>
  );
}
