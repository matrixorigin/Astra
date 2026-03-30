'use client';

import { useMemo, useState } from 'react';
import type { EventSummary } from '@/lib/models/platform';

export function EventLogViewer({
  events,
  emptyMessage,
}: {
  events: EventSummary[];
  emptyMessage: string;
}) {
  const [query, setQuery] = useState('');
  const [typeFilter, setTypeFilter] = useState('all');

  const eventTypes = useMemo(
    () => ['all', ...Array.from(new Set(events.map((event) => event.type)))],
    [events],
  );

  const filteredEvents = useMemo(() => {
    return events.filter((event) => {
      const matchesType = typeFilter === 'all' || event.type === typeFilter;
      const haystack = `${event.type} ${event.summary} ${event.sessionId}`.toLowerCase();
      const matchesQuery = haystack.includes(query.toLowerCase());
      return matchesType && matchesQuery;
    });
  }, [events, query, typeFilter]);

  return (
    <div className="space-y-4">
      <div className="grid gap-3 md:grid-cols-[1fr_220px]">
        <input
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Filter logs by event type, summary, or session id"
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none ring-0 placeholder:text-slate-500"
        />
        <select
          value={typeFilter}
          onChange={(event) => setTypeFilter(event.target.value)}
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
        >
          {eventTypes.map((type) => (
            <option key={type} value={type}>
              {type === 'all' ? 'All event types' : type}
            </option>
          ))}
        </select>
      </div>

      {filteredEvents.length === 0 ? (
        <div className="rounded-2xl border border-dashed border-slate-700 px-4 py-8 text-sm text-slate-400">
          {emptyMessage}
        </div>
      ) : (
        <div className="space-y-3">
          {filteredEvents.map((event) => (
            <div key={event.id} className="rounded-2xl border border-slate-800 bg-slate-950/70 p-4">
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div>
                  <p className="font-medium text-white">{event.type}</p>
                  <p className="mt-1 text-sm leading-6 text-slate-400">{event.summary}</p>
                </div>
                <div className="text-right text-xs text-slate-500">
                  <p>{event.createdAt}</p>
                  <p>{event.sessionId}</p>
                </div>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
