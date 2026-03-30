'use client';

import type { TokenUsage } from '@/lib/workspace/types';

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

export function TokenUsageBar({ usage }: { usage: TokenUsage }) {
  if (usage.totalTokens === 0) return null;

  return (
    <div className="flex items-center gap-4 border-t border-slate-800 bg-slate-950/80 px-4 py-1.5 text-[10px] text-slate-500">
      <span className="font-medium text-slate-400">Tokens</span>
      <span>
        ↑ {formatTokens(usage.promptTokens)}
      </span>
      <span>
        ↓ {formatTokens(usage.completionTokens)}
      </span>
      <span className="text-slate-400">
        Σ {formatTokens(usage.totalTokens)}
      </span>
      {usage.cacheReadTokens > 0 && (
        <span className="text-emerald-500/70">
          cache: {formatTokens(usage.cacheReadTokens)}
        </span>
      )}
    </div>
  );
}
