'use client';

import type { ConnectionState } from '@/lib/streaming/types';

const stateConfig: Record<ConnectionState, { label: string; dot: string; text: string }> = {
  disconnected: { label: 'Disconnected', dot: 'bg-slate-500', text: 'text-slate-400' },
  connecting: { label: 'Connecting…', dot: 'bg-amber-400 animate-pulse', text: 'text-amber-300' },
  connected: { label: 'Live', dot: 'bg-emerald-400', text: 'text-emerald-300' },
  error: { label: 'Error', dot: 'bg-red-500', text: 'text-red-400' },
};

export function ConnectionStatus({
  state,
  onReconnect,
}: {
  state: ConnectionState;
  onReconnect?: () => void;
}) {
  const cfg = stateConfig[state];

  return (
    <div className="flex items-center gap-2">
      <span className={`inline-block h-2 w-2 rounded-full ${cfg.dot}`} />
      <span className={`text-xs font-medium ${cfg.text}`}>{cfg.label}</span>
      {(state === 'disconnected' || state === 'error') && onReconnect ? (
        <button
          type="button"
          onClick={onReconnect}
          className="text-xs text-sky-400 hover:text-sky-300"
        >
          Reconnect
        </button>
      ) : null}
    </div>
  );
}
