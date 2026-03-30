'use client';

import { useState } from 'react';
import type { ToolCall } from '@/lib/workspace/types';

const TOOL_CATEGORIES: Record<string, { icon: string; label: string; color: string }> = {
  bash: { icon: '⬛', label: 'Terminal', color: 'text-green-400' },
  shell: { icon: '⬛', label: 'Terminal', color: 'text-green-400' },
  write_file: { icon: '📝', label: 'Write', color: 'text-sky-400' },
  str_replace: { icon: '✏️', label: 'Edit', color: 'text-sky-400' },
  multi_edit: { icon: '✏️', label: 'Edit', color: 'text-sky-400' },
  read_file: { icon: '📄', label: 'Read', color: 'text-slate-300' },
  list_dir: { icon: '📁', label: 'Browse', color: 'text-slate-300' },
  grep: { icon: '🔍', label: 'Search', color: 'text-amber-400' },
  glob: { icon: '🔍', label: 'Search', color: 'text-amber-400' },
  find_definition: { icon: '🔍', label: 'Search', color: 'text-amber-400' },
  find_references: { icon: '🔍', label: 'Search', color: 'text-amber-400' },
  symbol_search: { icon: '🔍', label: 'Search', color: 'text-amber-400' },
  git_status: { icon: '📊', label: 'Git', color: 'text-orange-400' },
  git_log: { icon: '📊', label: 'Git', color: 'text-orange-400' },
  git_diff: { icon: '📊', label: 'Git', color: 'text-orange-400' },
  git_commit: { icon: '📊', label: 'Git', color: 'text-orange-400' },
  memory_store: { icon: '🧠', label: 'Memory', color: 'text-violet-400' },
  memory_retrieve: { icon: '🧠', label: 'Memory', color: 'text-violet-400' },
};

function getToolMeta(tool: string) {
  if (TOOL_CATEGORIES[tool]) return TOOL_CATEGORIES[tool];
  // Fuzzy match
  if (tool.startsWith('git_')) return { icon: '📊', label: 'Git', color: 'text-orange-400' };
  if (tool.includes('search') || tool.includes('find')) return { icon: '🔍', label: 'Search', color: 'text-amber-400' };
  if (tool.includes('file') || tool.includes('read')) return { icon: '📄', label: 'File', color: 'text-slate-300' };
  if (tool.includes('memory')) return { icon: '🧠', label: 'Memory', color: 'text-violet-400' };
  return { icon: '⚙️', label: 'Tool', color: 'text-slate-400' };
}

function ToolCallRow({ toolCall }: { toolCall: ToolCall }) {
  const [expanded, setExpanded] = useState(false);
  const meta = getToolMeta(toolCall.tool);

  const duration =
    toolCall.finishedAt && toolCall.startedAt
      ? `${((toolCall.finishedAt - toolCall.startedAt) / 1000).toFixed(1)}s`
      : null;

  return (
    <div className="rounded-xl border border-slate-800 bg-slate-950/70 transition-all">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2.5 px-3 py-2.5 text-left text-sm"
      >
        {/* Status dot */}
        <span
          className={`inline-block h-2 w-2 shrink-0 rounded-full ${
            toolCall.status === 'running'
              ? 'animate-pulse bg-amber-400'
              : toolCall.status === 'done'
                ? 'bg-emerald-400'
                : 'bg-red-400'
          }`}
        />

        {/* Tool icon + name */}
        <span className="shrink-0 text-sm">{meta.icon}</span>
        <div className="min-w-0 flex-1">
          <span className={`font-mono text-xs ${meta.color}`}>{toolCall.tool}</span>
        </div>

        {/* Duration badge */}
        {duration ? (
          <span className="shrink-0 rounded-full bg-slate-800 px-2 py-0.5 text-[10px] text-slate-400">
            {duration}
          </span>
        ) : toolCall.status === 'running' ? (
          <span className="shrink-0 rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] text-amber-400">
            running
          </span>
        ) : null}

        <span className="shrink-0 text-xs text-slate-600">{expanded ? '▾' : '▸'}</span>
      </button>

      {expanded ? (
        <div className="border-t border-slate-800 px-3 py-2.5 text-xs">
          {toolCall.arguments ? (
            <div className="mb-2">
              <p className="mb-1 text-[10px] font-medium uppercase tracking-wide text-slate-500">
                Arguments
              </p>
              <pre className="max-h-40 overflow-auto rounded-lg bg-slate-900/50 p-2 whitespace-pre-wrap text-slate-300">
                {formatJSON(toolCall.arguments)}
              </pre>
            </div>
          ) : null}
          {toolCall.result ? (
            <div>
              <p className="mb-1 text-[10px] font-medium uppercase tracking-wide text-slate-500">
                Result
              </p>
              <pre className="max-h-56 overflow-auto rounded-lg bg-slate-900/50 p-2 whitespace-pre-wrap text-slate-300">
                {toolCall.result.length > 3000
                  ? toolCall.result.slice(0, 3000) + '\n… (truncated)'
                  : toolCall.result}
              </pre>
            </div>
          ) : toolCall.status === 'running' ? (
            <p className="animate-pulse text-slate-500">Executing…</p>
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
      <div className="flex flex-col items-center justify-center p-6 text-center">
        <div className="mb-2 flex h-10 w-10 items-center justify-center rounded-full bg-slate-800/50">
          <span className="text-lg">⚙️</span>
        </div>
        <p className="text-xs text-slate-500">
          Tool invocations will appear here as the agent works.
        </p>
      </div>
    );
  }

  const running = toolCalls.filter((tc) => tc.status === 'running').length;

  return (
    <div className="space-y-2 p-3">
      {running > 0 && (
        <div className="flex items-center gap-2 rounded-lg bg-amber-500/5 px-3 py-1.5 text-xs text-amber-400">
          <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />
          {running} tool{running > 1 ? 's' : ''} running
        </div>
      )}
      {toolCalls.map((tc) => (
        <ToolCallRow key={tc.callId} toolCall={tc} />
      ))}
    </div>
  );
}
