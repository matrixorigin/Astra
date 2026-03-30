'use client';

import type { PlanState } from '@/lib/workspace/types';

function ProgressBar({ completed, total }: { completed: number; total: number }) {
  const pct = total === 0 ? 0 : Math.round((completed / total) * 100);
  return (
    <div className="flex items-center gap-2">
      <div className="h-1.5 flex-1 rounded-full bg-slate-800">
        <div
          className="h-full rounded-full bg-sky-500 transition-all duration-500"
          style={{ width: `${pct}%` }}
        />
      </div>
      <span className="text-[10px] text-slate-500">{pct}%</span>
    </div>
  );
}

const STATUS_ICONS: Record<string, { icon: string; color: string }> = {
  done: { icon: '✓', color: 'text-emerald-400' },
  running: { icon: '●', color: 'text-amber-400 animate-pulse' },
  pending: { icon: '○', color: 'text-slate-600' },
  error: { icon: '✗', color: 'text-red-400' },
};

export function PlanProgressPanel({ plan }: { plan: PlanState }) {
  const completed = plan.subtasks.filter((s) => s.status === 'done').length;
  const total = plan.subtasks.length;

  return (
    <div className="space-y-3 p-3">
      {/* Plan header */}
      <div>
        <p className="text-xs font-medium uppercase tracking-wider text-slate-400">
          Plan
        </p>
        {plan.title && (
          <p className="mt-1 text-sm text-white">{plan.title}</p>
        )}
      </div>

      {/* Progress */}
      <div>
        <div className="mb-1 flex items-center justify-between text-[10px] text-slate-500">
          <span>{completed} / {total} tasks</span>
        </div>
        <ProgressBar completed={completed} total={total} />
      </div>

      {/* Subtask list */}
      <div className="space-y-1">
        {plan.subtasks.map((subtask) => {
          const { icon, color } = STATUS_ICONS[subtask.status] ?? STATUS_ICONS.pending;
          const isActive = subtask.status === 'running';

          return (
            <div
              key={subtask.id}
              className={`flex items-start gap-2 rounded-md px-2 py-1.5 text-xs ${
                isActive ? 'bg-amber-500/5 border border-amber-500/20' : ''
              }`}
            >
              <span className={`mt-0.5 flex-shrink-0 font-mono text-[11px] ${color}`}>
                {icon}
              </span>
              <span
                className={
                  subtask.status === 'done'
                    ? 'text-slate-500 line-through'
                    : isActive
                      ? 'text-amber-200'
                      : 'text-slate-300'
                }
              >
                {subtask.title}
              </span>
            </div>
          );
        })}
      </div>
    </div>
  );
}
