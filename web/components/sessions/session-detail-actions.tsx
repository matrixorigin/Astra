'use client';

import { useState, useTransition } from 'react';
import type { SessionSummary } from '@/lib/models/platform';
import {
  resumeSessionAction,
  cancelSessionAction,
  closeSessionAction,
} from '@/lib/actions/session-actions';

export function SessionDetailActions({ session }: { session: SessionSummary }) {
  const [isPending, startTransition] = useTransition();
  const [message, setMessage] = useState<string | null>(null);

  const handleAction = (action: (id: string) => Promise<{ ok: boolean; error?: string }>) => {
    setMessage(null);
    startTransition(async () => {
      const result = await action(session.id);
      if (!result.ok) {
        setMessage(result.error ?? 'Action failed');
      }
    });
  };

  const canResume = session.status === 'paused' || session.status === 'waiting';
  const canCancel =
    session.status === 'active' || session.status === 'paused' || session.status === 'waiting';
  const canClose = session.status !== 'closed' && session.status !== 'cancelled';

  return (
    <>
      {canResume ? (
        <button
          type="button"
          disabled={isPending}
          onClick={() => handleAction(resumeSessionAction)}
          className="rounded-full border border-emerald-700/60 px-3 py-1 text-xs text-emerald-300 hover:border-emerald-500 hover:text-emerald-200 disabled:opacity-50"
        >
          {isPending ? '…' : 'Resume'}
        </button>
      ) : null}
      {canCancel ? (
        <button
          type="button"
          disabled={isPending}
          onClick={() => handleAction(cancelSessionAction)}
          className="rounded-full border border-amber-700/60 px-3 py-1 text-xs text-amber-300 hover:border-amber-500 hover:text-amber-200 disabled:opacity-50"
        >
          {isPending ? '…' : 'Cancel'}
        </button>
      ) : null}
      {canClose ? (
        <button
          type="button"
          disabled={isPending}
          onClick={() => handleAction(closeSessionAction)}
          className="rounded-full border border-red-700/60 px-3 py-1 text-xs text-red-300 hover:border-red-500 hover:text-red-200 disabled:opacity-50"
        >
          {isPending ? '…' : 'Close'}
        </button>
      ) : null}
      {message ? <span className="text-xs text-red-400">{message}</span> : null}
    </>
  );
}
