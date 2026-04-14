'use client';

import type { EdgeConnection } from '@/hooks/use-edge-connections';

function formatDuration(secs: number): string {
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
}

export function EdgeConnectionBadge({
  edges,
  hasEdge,
}: {
  edges: EdgeConnection[];
  hasEdge: boolean;
}) {
  if (!hasEdge) return null;

  const first = edges[0];
  const label = first.hostname ?? first.edge_agent_id.slice(0, 8);

  return (
    <div className="group relative flex items-center gap-1.5">
      <span className="inline-block h-2 w-2 rounded-full bg-violet-400" />
      <span className="text-xs font-medium text-violet-300">
        Edge: {label}
        {edges.length > 1 ? ` +${edges.length - 1}` : ''}
      </span>

      {/* Hover tooltip with details */}
      <div className="pointer-events-none absolute left-0 top-full z-50 mt-2 hidden w-64 rounded-lg border border-slate-700 bg-slate-900 p-3 shadow-xl group-hover:block">
        <p className="mb-2 text-xs font-semibold text-slate-300">
          Connected Edge Agents ({edges.length})
        </p>
        <div className="space-y-2">
          {edges.map((edge) => (
            <div
              key={edge.edge_agent_id}
              className="rounded-md border border-slate-800 bg-slate-950 px-2 py-1.5"
            >
              <p className="text-xs font-medium text-white">
                {edge.hostname ?? 'Unknown host'}
              </p>
              <div className="mt-0.5 flex items-center gap-2 text-[10px] text-slate-500">
                <span title={edge.edge_agent_id}>
                  {edge.edge_agent_id.slice(0, 12)}…
                </span>
                <span>·</span>
                <span>{formatDuration(edge.connected_secs)}</span>
                {edge.workspace_dir ? (
                  <>
                    <span>·</span>
                    <span className="truncate" title={edge.workspace_dir}>
                      {edge.workspace_dir}
                    </span>
                  </>
                ) : null}
              </div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
