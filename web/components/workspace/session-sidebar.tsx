'use client';

import { useState, useEffect, useCallback } from 'react';

type SessionItem = {
  session_id: string;
  title: string | null;
  status: string;
  event_count: number;
  created_at: string;
  agent_id: string | null;
};

export function SessionSidebar({
  currentSessionId,
  onSelectSession,
  onNewSession,
  collapsed,
  onToggle,
}: {
  currentSessionId: string | null;
  onSelectSession: (sessionId: string) => void;
  onNewSession: () => void;
  collapsed: boolean;
  onToggle: () => void;
}) {
  const [sessions, setSessions] = useState<SessionItem[]>([]);
  const [loading, setLoading] = useState(false);

  const fetchSessions = useCallback(async () => {
    setLoading(true);
    try {
      const res = await fetch(`/api/backend/sessions?limit=50`);
      if (!res.ok) return;
      const data = await res.json();
      const items: SessionItem[] = Array.isArray(data)
        ? data
        : Array.isArray(data.sessions)
          ? data.sessions
          : Array.isArray(data.data)
            ? data.data
            : [];
      setSessions(items);
    } catch {
      // Fail silently
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchSessions();
  }, [fetchSessions]);

  const handleAction = useCallback(
    async (sessionId: string, action: 'close' | 'resume') => {
      try {
        await fetch(`/api/backend/sessions/${sessionId}/${action}`, {
          method: 'POST',
        });
        fetchSessions();
      } catch {
        // Fail silently
      }
    },
    [fetchSessions],
  );

  if (collapsed) {
    return (
      <div className="flex w-12 flex-col items-center border-r border-slate-800 bg-slate-950/60 py-3">
        <button
          type="button"
          onClick={onToggle}
          className="rounded-lg p-2 text-slate-400 hover:bg-slate-800 hover:text-white"
          title="Show sessions"
        >
          <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
          </svg>
        </button>
        <button
          type="button"
          onClick={onNewSession}
          className="mt-2 rounded-lg p-2 text-slate-400 hover:bg-slate-800 hover:text-sky-400"
          title="New session"
        >
          <svg className="h-5 w-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 4v16m8-8H4" />
          </svg>
        </button>
      </div>
    );
  }

  return (
    <div className="flex w-64 flex-col border-r border-slate-800 bg-slate-950/60">
      {/* Header */}
      <div className="flex items-center justify-between border-b border-slate-800 px-3 py-3">
        <span className="text-xs font-medium uppercase tracking-wider text-slate-400">
          Sessions
        </span>
        <div className="flex gap-1">
          <button
            type="button"
            onClick={onNewSession}
            className="rounded-md p-1.5 text-slate-400 hover:bg-slate-800 hover:text-sky-400"
            title="New session"
          >
            <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 4v16m8-8H4" />
            </svg>
          </button>
          <button
            type="button"
            onClick={fetchSessions}
            className="rounded-md p-1.5 text-slate-400 hover:bg-slate-800 hover:text-white"
            title="Refresh"
          >
            <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
            </svg>
          </button>
          <button
            type="button"
            onClick={onToggle}
            className="rounded-md p-1.5 text-slate-400 hover:bg-slate-800 hover:text-white"
            title="Collapse"
          >
            <svg className="h-4 w-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 19l-7-7 7-7" />
            </svg>
          </button>
        </div>
      </div>

      {/* Session list */}
      <div className="flex-1 overflow-y-auto">
        {loading && sessions.length === 0 ? (
          <div className="space-y-2 p-3">
            {[1, 2, 3].map((i) => (
              <div key={i} className="h-16 animate-pulse rounded-lg bg-slate-800/50" />
            ))}
          </div>
        ) : sessions.length === 0 ? (
          <div className="p-4 text-center text-xs text-slate-500">
            No sessions yet. Start a conversation!
          </div>
        ) : (
          <div className="space-y-1 p-2">
            {sessions.map((s) => {
              const isActive = s.session_id === currentSessionId;
              const statusColor =
                s.status === 'active'
                  ? 'bg-emerald-400'
                  : s.status === 'closed'
                    ? 'bg-slate-500'
                    : 'bg-amber-400';

              return (
                <div
                  key={s.session_id}
                  role="button"
                  tabIndex={0}
                  onClick={() => onSelectSession(s.session_id)}
                  onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') onSelectSession(s.session_id); }}
                  className={`group w-full cursor-pointer rounded-lg px-3 py-2.5 text-left transition-colors ${
                    isActive
                      ? 'bg-sky-600/15 border border-sky-500/30'
                      : 'hover:bg-slate-800/50 border border-transparent'
                  }`}
                >
                  <div className="flex items-start justify-between gap-2">
                    <div className="min-w-0 flex-1">
                      <p className={`truncate text-sm ${isActive ? 'text-sky-200' : 'text-slate-200'}`}>
                        {s.title || `Session ${s.session_id.slice(0, 8)}…`}
                      </p>
                      <div className="mt-1 flex items-center gap-2 text-[10px] text-slate-500">
                        <span className={`inline-block h-1.5 w-1.5 rounded-full ${statusColor}`} />
                        <span>{s.status}</span>
                        <span>·</span>
                        <span>{s.event_count} events</span>
                      </div>
                    </div>
                    {/* Action buttons on hover */}
                    <div className="flex-shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
                      {s.status === 'active' ? (
                        <button
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation();
                            handleAction(s.session_id, 'close');
                          }}
                          className="rounded p-1 text-slate-500 hover:bg-slate-700 hover:text-red-400"
                          title="Close session"
                        >
                          <svg className="h-3 w-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M6 18L18 6M6 6l12 12" />
                          </svg>
                        </button>
                      ) : s.status === 'closed' ? (
                        <button
                          type="button"
                          onClick={(e) => {
                            e.stopPropagation();
                            handleAction(s.session_id, 'resume');
                          }}
                          className="rounded p-1 text-slate-500 hover:bg-slate-700 hover:text-emerald-400"
                          title="Resume session"
                        >
                          <svg className="h-3 w-3" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M14.752 11.168l-3.197-2.132A1 1 0 0010 9.87v4.263a1 1 0 001.555.832l3.197-2.132a1 1 0 000-1.664z" />
                          </svg>
                        </button>
                      ) : null}
                    </div>
                  </div>
                  <p className="mt-0.5 text-[10px] text-slate-600">
                    {new Date(s.created_at).toLocaleDateString(undefined, {
                      month: 'short',
                      day: 'numeric',
                      hour: '2-digit',
                      minute: '2-digit',
                    })}
                  </p>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
