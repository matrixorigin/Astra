'use client';

import { useEffect } from 'react';
import Link from 'next/link';

export default function WorkspaceError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  useEffect(() => {
    console.error('[workspace error]', error);
  }, [error]);

  return (
    <div className="flex min-h-[50vh] flex-col items-center justify-center gap-4">
      <div className="max-w-md rounded-2xl border border-red-800/50 bg-red-950/20 p-8 text-center">
        <h2 className="text-lg font-semibold text-red-300">Workspace error</h2>
        <p className="mt-2 text-sm text-slate-400">
          {error.message || 'Failed to load the workspace. The backend may be unreachable.'}
        </p>
        <div className="mt-4 flex justify-center gap-3">
          <button
            type="button"
            onClick={reset}
            className="rounded-xl bg-sky-600 px-5 py-2.5 text-sm font-medium text-white hover:bg-sky-500"
          >
            Retry
          </button>
          <Link
            href="/sessions"
            className="rounded-xl border border-slate-700 px-5 py-2.5 text-sm font-medium text-slate-300 hover:border-slate-500"
          >
            Back to sessions
          </Link>
        </div>
      </div>
    </div>
  );
}
