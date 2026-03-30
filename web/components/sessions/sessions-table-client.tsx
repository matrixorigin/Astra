'use client';

import Link from 'next/link';
import { useMemo, useState } from 'react';
import type { SessionSummary } from '@/lib/models/platform';

export function SessionsTableClient({ sessions }: { sessions: SessionSummary[] }) {
  const [query, setQuery] = useState('');
  const [statusFilter, setStatusFilter] = useState('all');

  const statuses = useMemo(
    () => ['all', ...Array.from(new Set(sessions.map((session) => session.status)))],
    [sessions],
  );

  const filteredSessions = useMemo(() => {
    return sessions.filter((session) => {
      const matchesStatus = statusFilter === 'all' || session.status === statusFilter;
      const haystack =
        `${session.title} ${session.id} ${session.owner} ${session.agentId ?? ''}`.toLowerCase();
      const matchesQuery = haystack.includes(query.toLowerCase());
      return matchesStatus && matchesQuery;
    });
  }, [query, sessions, statusFilter]);

  return (
    <div className="space-y-4">
      <div className="grid gap-3 md:grid-cols-[1fr_220px]">
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search by title, session id, owner, or agent id"
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500"
        />
        <select
          value={statusFilter}
          onChange={(event) => setStatusFilter(event.target.value)}
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
        >
          {statuses.map((status) => (
            <option key={status} value={status}>
              {status === 'all' ? 'All statuses' : status}
            </option>
          ))}
        </select>
      </div>

      <div className="overflow-hidden rounded-2xl border border-slate-800">
        <table className="min-w-full divide-y divide-slate-800 text-left text-sm">
          <thead className="bg-slate-950/80 text-slate-400">
            <tr>
              <th className="px-4 py-3 font-medium">Session</th>
              <th className="px-4 py-3 font-medium">Owner</th>
              <th className="px-4 py-3 font-medium">Status</th>
              <th className="px-4 py-3 font-medium">Events</th>
              <th className="px-4 py-3 font-medium">Updated</th>
              <th className="px-4 py-3 font-medium">Actions</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-slate-800 bg-slate-950/40">
            {filteredSessions.map((session) => (
              <tr key={session.id}>
                <td className="px-4 py-4">
                  <p className="font-medium text-white">{session.title}</p>
                  <p className="text-slate-500">{session.id}</p>
                </td>
                <td className="px-4 py-4 text-slate-300">{session.owner}</td>
                <td className="px-4 py-4 text-slate-300">{session.status}</td>
                <td className="px-4 py-4 text-slate-300">{session.eventCount}</td>
                <td className="px-4 py-4 text-slate-300">{session.updatedAt ?? session.createdAt}</td>
                <td className="px-4 py-4">
                  <div className="flex flex-wrap gap-2">
                    <Link
                      href={`/sessions/${session.id}`}
                      className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-200 hover:border-sky-400/40 hover:text-sky-300"
                    >
                      Details
                    </Link>
                    <Link
                      href={`/workspace?sessionId=${session.id}`}
                      className="rounded-full border border-slate-700 px-3 py-1 text-xs text-slate-200 hover:border-sky-400/40 hover:text-sky-300"
                    >
                      Workspace
                    </Link>
                  </div>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
