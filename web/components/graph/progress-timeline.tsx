'use client';

import type { PlanProgressEvent } from '@/lib/graph/types';
import { statusColors } from '@/lib/graph/layout';

interface ProgressTimelineProps {
  events: PlanProgressEvent[];
}

const actionIcons: Record<string, string> = {
  started: '▶️',
  completed: '✅',
  skipped: '⏭️',
  plan_complete: '🎉',
  plan_paused: '⏸️',
};

function relativeTime(ts: string): string {
  if (!ts) return '';
  const diff = Date.now() - new Date(ts).getTime();
  if (diff < 60_000) return 'just now';
  if (diff < 3600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86400_000) return `${Math.floor(diff / 3600_000)}h ago`;
  return `${Math.floor(diff / 86400_000)}d ago`;
}

export function ProgressTimeline({ events }: ProgressTimelineProps) {
  if (events.length === 0) {
    return (
      <div className="rounded-xl border border-dashed border-slate-700 p-6 text-center text-sm text-slate-500">
        No progress events yet
      </div>
    );
  }

  return (
    <div className="space-y-0">
      {events.map((ev, i) => {
        const statusKey = ev.action === 'completed' ? 'completed' : ev.action === 'started' ? 'in_progress' : 'pending';
        const palette = statusColors[statusKey] ?? statusColors.pending;

        return (
          <div key={`${ev.subtaskId}-${ev.action}-${i}`} className="flex gap-3">
            {/* Timeline line */}
            <div className="flex flex-col items-center">
              <div
                className="flex h-7 w-7 items-center justify-center rounded-full border-2 text-xs"
                style={{ borderColor: palette.border, backgroundColor: palette.bg }}
              >
                {actionIcons[ev.action] ?? '•'}
              </div>
              {i < events.length - 1 && (
                <div className="h-8 w-px bg-slate-700" />
              )}
            </div>

            {/* Content */}
            <div className="pb-6">
              <p className="text-sm font-medium text-white">{ev.subtaskTitle}</p>
              <p className="text-xs text-slate-400">
                <span className="capitalize">{ev.action.replace('_', ' ')}</span>
                {' · '}
                {ev.completedSubtasks}/{ev.totalSubtasks} done
                {' · '}
                {ev.progressPct}%
              </p>
              <p className="mt-0.5 text-xs text-slate-500">{relativeTime(ev.timestamp)}</p>
            </div>
          </div>
        );
      })}
    </div>
  );
}
