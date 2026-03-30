'use client';

import { useMemo, useState } from 'react';
import type { AgentSummary } from '@/lib/models/platform';

const statusColors: Record<string, string> = {
  active: '#22c55e',
  inactive: '#6b7280',
};

export function AgentsTableClient({ agents }: { agents: AgentSummary[] }) {
  const [query, setQuery] = useState('');
  const [statusFilter, setStatusFilter] = useState('all');
  const [typeFilter, setTypeFilter] = useState('all');

  const statuses = useMemo(
    () => ['all', ...Array.from(new Set(agents.map((a) => a.status)))],
    [agents],
  );

  const types = useMemo(
    () => ['all', ...Array.from(new Set(agents.map((a) => a.type)))],
    [agents],
  );

  const filtered = useMemo(() => {
    return agents.filter((agent) => {
      const matchesStatus = statusFilter === 'all' || agent.status === statusFilter;
      const matchesType = typeFilter === 'all' || agent.type === typeFilter;
      const haystack =
        `${agent.name} ${agent.type} ${agent.model} ${agent.owner} ${agent.skills.join(' ')}`.toLowerCase();
      const matchesQuery = haystack.includes(query.toLowerCase());
      return matchesStatus && matchesType && matchesQuery;
    });
  }, [agents, query, statusFilter, typeFilter]);

  return (
    <div className="space-y-4">
      <div className="grid gap-3 md:grid-cols-[1fr_160px_160px]">
        <input
          type="search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder="Search agents by name, type, model, skills…"
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none ring-0 placeholder:text-slate-500"
        />
        <select
          value={statusFilter}
          onChange={(e) => setStatusFilter(e.target.value)}
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
        >
          {statuses.map((s) => (
            <option key={s} value={s}>
              {s === 'all' ? 'All statuses' : s}
            </option>
          ))}
        </select>
        <select
          value={typeFilter}
          onChange={(e) => setTypeFilter(e.target.value)}
          className="rounded-2xl border border-slate-800 bg-slate-950/70 px-4 py-3 text-sm text-white outline-none"
        >
          {types.map((t) => (
            <option key={t} value={t}>
              {t === 'all' ? 'All types' : t}
            </option>
          ))}
        </select>
      </div>

      <p className="text-xs text-slate-500">
        {filtered.length} of {agents.length} agent{agents.length !== 1 ? 's' : ''}
      </p>

      {filtered.length === 0 ? (
        <div className="rounded-2xl border border-dashed border-slate-700 px-4 py-8 text-center text-sm text-slate-400">
          No agents match the current filters.
        </div>
      ) : (
        <div className="space-y-4">
          {filtered.map((agent) => {
            const color = statusColors[agent.status] ?? '#475569';
            return (
              <div key={agent.id} className="rounded-2xl border border-slate-800 bg-slate-950/70 p-5">
                <div className="flex flex-wrap items-center justify-between gap-3">
                  <div>
                    <h2 className="text-lg font-semibold text-white">{agent.name}</h2>
                    <p className="text-sm text-slate-400">
                      {agent.type} · model {agent.model} · owned by {agent.owner}
                    </p>
                  </div>
                  <span
                    className="rounded-full px-3 py-1 text-xs font-medium"
                    style={{
                      backgroundColor: `${color}20`,
                      color,
                      border: `1px solid ${color}40`,
                    }}
                  >
                    {agent.status}
                  </span>
                </div>
                {agent.skills.length > 0 && (
                  <div className="mt-3 flex flex-wrap gap-2">
                    {agent.skills.map((skill) => (
                      <span key={skill} className="rounded-full bg-slate-800 px-3 py-1 text-xs text-slate-300">
                        {skill}
                      </span>
                    ))}
                  </div>
                )}
                {agent.updatedAt && (
                  <p className="mt-2 text-xs text-slate-500">Updated: {agent.updatedAt}</p>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
