'use client';

import { useState } from 'react';
import type { ToolCall } from '@/lib/workspace/types';

function ToolCallRow({ toolCall }: { toolCall: ToolCall }) {
  const [expanded, setExpanded] = useState(false);

  const duration =
    toolCall.finishedAt && toolCall.startedAt
      ? `${((toolCall.finishedAt - toolCall.startedAt) / 1000).toFixed(1)}s`
      : null;

  return (
    <div className="rounded-xl border border-slate-800 bg-slate-950/70">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-3 px-4 py-3 text-left text-sm"
      >
        <span
          className={`inline-block h-2 w-2 shrink-0 rounded-full ${
            toolCall.status === 'running'
              ? 'animate-pulse bg-amber-400'
              : toolCall.status === 'done'
                ? 'bg-emerald-400'
                : 'bg-red-400'
          }`}
        />
        <span className="min-w-0 flex-1 font-mono text-white">{toolCall.tool}</span>
        {duration ? (
          <span className="shrink-0 text-xs text-slate-500">{duration}</span>
        ) : null}
        <span className="shrink-0 text-slate-500">{expanded ? '▾' : '▸'}</span>
      </button>

      {expanded ? (
        <div className="border-t border-slate-800 px-4 py-3 text-xs">
          {toolCall.arguments ? (
            <div className="mb-2">
              <p className="text-slate-500">Arguments</p>
              <pre className="mt-1 max-h-32 overflow-auto whitespace-pre-wrap text-slate-300">
                {formatJSON(toolCall.arguments)}
              </pre>
            </div>
          ) : null}
          {toolCall.result ? (
            <div>
              <p className="text-slate-500">Result</p>
              <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap text-slate-300">
                {toolCall.result.length > 2000
                  ? toolCall.result.slice(0, 2000) + '\n… (truncated)'
                  : toolCall.result}
              </pre>
            </div>
          ) : toolCall.status === 'running' ? (
            <p className="animate-pulse text-slate-500">Running…</p>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}

function formatJSON(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}

export function ToolTimeline({ toolCalls }: { toolCalls: ToolCall[] }) {
  if (toolCalls.length === 0) {
    return (
      <p className="p-4 text-sm text-slate-500">
        No tool calls yet. Tool invocations will appear here as the agent works.
      </p>
    );
  }

  return (
    <div className="space-y-2 p-4">
      {toolCalls.map((tc) => (
        <ToolCallRow key={tc.callId} toolCall={tc} />
      ))}
    </div>
  );
}
