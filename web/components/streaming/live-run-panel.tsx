'use client';

import { useRef, useEffect } from 'react';
import type { StreamEvent } from '@/lib/streaming/types';
import type { ConnectionState } from '@/lib/streaming/types';
import { ConnectionStatus } from './connection-status';

function eventBadgeColor(event: StreamEvent): string {
  if (event.type === 'run_finished') {
    if (event.status === 'failed') return 'bg-red-500/20 text-red-300';
    if (event.status === 'cancelled') return 'bg-amber-500/20 text-amber-300';
  }

  if (event.type === 'agent_completed') {
    if (event.status === 'failed') return 'bg-red-500/20 text-red-300';
    if (event.status === 'cancelled') return 'bg-amber-500/20 text-amber-300';
  }

  const colors: Record<string, string> = {
    text_delta: 'bg-sky-500/20 text-sky-300',
    run_started: 'bg-blue-500/20 text-blue-300',
    run_paused: 'bg-amber-500/20 text-amber-300',
    run_resumed: 'bg-blue-500/20 text-blue-300',
    run_finished: 'bg-emerald-500/20 text-emerald-300',
    run_cancelled: 'bg-amber-500/20 text-amber-300',
    tool_call_start: 'bg-violet-500/20 text-violet-300',
    tool_call_end: 'bg-violet-500/20 text-violet-300',
    usage: 'bg-slate-500/20 text-slate-300',
    turn_complete: 'bg-emerald-500/20 text-emerald-300',
    session_info: 'bg-blue-500/20 text-blue-300',
    error: 'bg-red-500/20 text-red-300',
    warning: 'bg-amber-500/20 text-amber-300',
    explain: 'bg-slate-500/20 text-slate-400',
    agent_spawned: 'bg-teal-500/20 text-teal-300',
    agent_delegated: 'bg-teal-500/20 text-teal-300',
    agent_progress: 'bg-cyan-500/20 text-cyan-300',
    agent_completed: 'bg-emerald-500/20 text-emerald-300',
  };

  return colors[event.type] ?? 'bg-slate-700/30 text-slate-400';
}

function EventBadge({ event }: { event: StreamEvent }) {
  return (
    <span
      className={`inline-block rounded-full px-2 py-0.5 text-xs font-medium ${eventBadgeColor(event)}`}
    >
      {event.type}
    </span>
  );
}

function eventSummary(event: StreamEvent): string {
  switch (event.type) {
    case 'run_started':
      return event.run_id ? `Run started (${event.run_id})` : 'Run started';
    case 'run_paused':
      return event.run_id ? `Run paused (${event.run_id})` : 'Run paused';
    case 'run_resumed':
      return event.run_id ? `Run resumed (${event.run_id})` : 'Run resumed';
    case 'run_finished':
      return event.error
        ? `Run ${event.status ?? 'finished'}: ${event.error}`
        : `Run ${event.status ?? 'finished'}`;
    case 'run_cancelled':
      return `Run cancelled (${event.run_id})`;
    case 'text_delta':
      return event.content.length > 120
        ? event.content.slice(0, 120) + '…'
        : event.content;
    case 'tool_call_start':
      return `${event.tool}(${event.call_id})`;
    case 'tool_call_end':
      return event.result
        ? event.result.slice(0, 80) + (event.result.length > 80 ? '…' : '')
        : `call ${event.call_id} done`;
    case 'usage':
      return `${event.prompt_tokens} prompt · ${event.completion_tokens} completion`;
    case 'turn_complete':
      return 'Turn finished';
    case 'session_info':
      return `session=${event.session_id}${event.run_id ? ` run=${event.run_id}` : ''}`;
    case 'error':
      return event.message;
    case 'warning':
      return event.message;
    case 'explain':
      return event.content.slice(0, 120) + (event.content.length > 120 ? '…' : '');
    case 'reasoning_delta':
      return 'Thinking…';
    case 'reasoning_done':
      return 'Thinking complete';
    case 'plan_created':
      return `Plan: ${event.plan.title ?? 'created'} (${event.plan.subtasks.length} tasks)`;
    case 'plan_revised':
      return `Plan revised (${event.plan.subtasks.length} tasks)`;
    case 'plan_step_start':
      return `Step: ${event.step}`;
    case 'plan_step_done':
      return `Step done: ${event.step}`;
    case 'agent_delegated':
      return `Delegated to ${event.agent_id}: ${event.task}`;
    case 'agent_spawned':
      return `▶ Agent ${event.agent_id} spawned (${event.agent_type}) ← ${event.parent_run_id.slice(0, 8)}`;
    case 'agent_progress':
      return `Agent ${event.agent_id}: ${event.status}${event.tool_name ? ` [${event.tool_name}]` : ''}${event.description ? ` — ${event.description}` : ''}`;
    case 'agent_completed':
      return `Agent ${event.agent_id} ${event.status}${event.result_summary ? `: ${event.result_summary}` : ''}${event.error ? ` — ${event.error}` : ''}`;
    default:
      return event.type;
  }
}

export function LiveRunPanel({
  events,
  connectionState,
  onReconnect,
  onDisconnect,
  title = 'Live stream',
}: {
  events: StreamEvent[];
  connectionState: ConnectionState;
  onReconnect?: () => void;
  onDisconnect?: () => void;
  title?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);

  // Auto-scroll to bottom when new events arrive
  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [events.length]);

  return (
    <div className="rounded-2xl border border-slate-800 bg-slate-950/70">
      <div className="flex items-center justify-between border-b border-slate-800 px-4 py-3">
        <div className="flex items-center gap-3">
          <h3 className="text-sm font-medium text-white">{title}</h3>
          <ConnectionStatus state={connectionState} onReconnect={onReconnect} />
        </div>
        <div className="flex items-center gap-2">
          <span className="text-xs text-slate-500">{events.length} events</span>
          {connectionState === 'connected' && onDisconnect ? (
            <button
              type="button"
              onClick={onDisconnect}
              className="text-xs text-slate-400 hover:text-slate-200"
            >
              Pause
            </button>
          ) : null}
        </div>
      </div>

      <div ref={scrollRef} className="max-h-96 overflow-y-auto p-4">
        {events.length === 0 ? (
          <p className="text-sm text-slate-500">
            {connectionState === 'connected'
              ? 'Waiting for events…'
              : 'Not connected. Events will appear here once streaming starts.'}
          </p>
        ) : (
          <div className="space-y-2">
            {events.map((event, index) => (
              <div
                key={index}
                className="flex items-start gap-2 text-sm"
              >
                <EventBadge event={event} />
                <span className="min-w-0 flex-1 break-words text-slate-300">
                  {eventSummary(event)}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}
