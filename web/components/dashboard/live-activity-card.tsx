'use client';

import { useCallback } from 'react';
import Link from 'next/link';
import { usePolling } from '@/hooks/use-polling';

type RunListResponse = {
  runs: Array<{
    run_id: string;
    status: string;
  }>;
  total: number;
};

export function LiveActivityCard() {
  const fetcher = useCallback(async () => {
    const res = await fetch('/api/backend/runs?limit=5&offset=0', {
      headers: {
        'Content-Type': 'application/json',
      },
      cache: 'no-store',
    });
    if (!res.ok) throw new Error(`Failed to fetch runs: ${res.status}`);
    return (await res.json()) as RunListResponse;
  }, []);

  const { data } = usePolling<RunListResponse>({
    fetcher,
    intervalMs: 15_000,
    enabled: true,
  });

  const activeCount =
    data?.runs.filter((r) => r.status === 'running' || r.status === 'waiting').length ?? 0;

  return (
    <Link
      href="/runs"
      className="block rounded-2xl border border-slate-800 bg-slate-900/50 p-5 transition-colors hover:border-slate-600"
    >
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          {activeCount > 0 ? (
            <span className="relative flex h-3 w-3">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-emerald-400 opacity-75" />
              <span className="relative inline-flex h-3 w-3 rounded-full bg-emerald-500" />
            </span>
          ) : (
            <span className="inline-flex h-3 w-3 rounded-full bg-slate-600" />
          )}
          <span className="text-sm font-semibold text-white">Live Activity</span>
        </div>
        <span className="text-xs text-slate-500">→ Runs page</span>
      </div>
      <p className="mt-3 text-2xl font-bold text-white">
        {activeCount}
        <span className="ml-2 text-sm font-normal text-slate-400">
          active run{activeCount !== 1 ? 's' : ''}
        </span>
      </p>
      <p className="mt-1 text-xs text-slate-500">
        {activeCount > 0
          ? 'In-progress runs detected — click to view live stream'
          : 'No runs currently in progress'}
      </p>
    </Link>
  );
}
