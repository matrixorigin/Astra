'use client';

import { useState } from 'react';
import type { ThinkingBlock as ThinkingBlockType } from '@/lib/workspace/types';

export function ThinkingBlock({ thinking }: { thinking: ThinkingBlockType }) {
  const [expanded, setExpanded] = useState(!thinking.done);

  return (
    <div className="mb-2 rounded-lg border border-slate-700/50 bg-slate-900/40">
      <button
        type="button"
        onClick={() => setExpanded(!expanded)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        {/* Thinking indicator */}
        {!thinking.done ? (
          <span className="flex h-4 w-4 items-center justify-center">
            <span className="h-2 w-2 animate-pulse rounded-full bg-violet-400" />
          </span>
        ) : (
          <svg
            className={`h-4 w-4 text-slate-500 transition-transform ${expanded ? 'rotate-90' : ''}`}
            fill="none"
            viewBox="0 0 24 24"
            stroke="currentColor"
          >
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9 5l7 7-7 7" />
          </svg>
        )}

        <span className="text-xs font-medium text-slate-400">
          {thinking.done ? 'Thought process' : 'Thinking…'}
        </span>

        {thinking.done && (
          <span className="text-[10px] text-slate-600">
            {thinking.content.length > 200
              ? `${Math.ceil(thinking.content.length / 4)} words`
              : ''}
          </span>
        )}
      </button>

      {expanded && (
        <div className="border-t border-slate-800/50 px-3 py-2">
          <div className="max-h-64 overflow-y-auto text-xs leading-relaxed text-slate-400/80 whitespace-pre-wrap">
            {thinking.content || (
              <span className="animate-pulse text-slate-500">Processing…</span>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
