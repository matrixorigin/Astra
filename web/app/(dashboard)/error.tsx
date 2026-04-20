'use client';

import { useEffect } from 'react';

import { logger } from '@/lib/logger';

export default function DashboardError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  useEffect(() => {
    logger.error('dashboard error boundary', {
      name: error.name,
      message: error.message,
      digest: error.digest,
    });
  }, [error]);

  return (
    <div className="flex min-h-[40vh] flex-col items-center justify-center gap-4">
      <div className="rounded-2xl border border-red-800/50 bg-red-950/20 p-8 text-center">
        <h2 className="text-lg font-semibold text-red-300">Something went wrong</h2>
        <p className="mt-2 max-w-md text-sm text-slate-400">
          {error.message || 'An unexpected error occurred while loading this page.'}
        </p>
        {error.digest ? (
          <p className="mt-2 text-xs text-slate-500">Error ID: {error.digest}</p>
        ) : null}
        <button
          type="button"
          onClick={reset}
          className="mt-4 rounded-xl bg-sky-600 px-5 py-2.5 text-sm font-medium text-white hover:bg-sky-500"
        >
          Try again
        </button>
      </div>
    </div>
  );
}
