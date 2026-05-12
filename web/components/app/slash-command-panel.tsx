'use client';

import { Bot, Loader2, Puzzle, Sparkles } from 'lucide-react';
import type { SlashCommandItem } from '@/lib/composer/slash-commands';
import { cn } from '@/lib/utils/cn';

type SlashCommandPanelProps = {
  items: SlashCommandItem[];
  loading: boolean;
  error: string | null;
  activeIndex: number;
  onSelect: (item: SlashCommandItem) => void;
};

const KIND_ICON = {
  skill: Puzzle,
  mode: Bot,
  action: Sparkles,
} satisfies Record<SlashCommandItem['kind'], React.ComponentType<{ className?: string }>>;

function subtitle(item: SlashCommandItem) {
  if (item.description) {
    return item.description;
  }
  if (item.kind === 'skill') {
    return 'Skill';
  }
  if (item.kind === 'mode') {
    return 'Mode';
  }
  return 'Command';
}

export function SlashCommandPanel({
  items,
  loading,
  error,
  activeIndex,
  onSelect,
}: SlashCommandPanelProps) {
  return (
    <div className="absolute bottom-full left-8 z-40 mb-2 w-80 max-w-[calc(100vw-3rem)] rounded-[14px] border border-border bg-surface p-2 shadow-[0_0.75rem_2rem_rgba(28,25,23,0.14)]">
      {loading ? (
        <div className="flex items-center gap-3 rounded-control px-3 py-2 text-sm text-text-secondary">
          <Loader2 className="size-4 animate-spin" />
          Loading commands...
        </div>
      ) : null}

      {error ? (
        <div className="rounded-control border border-danger/20 bg-danger/5 px-3 py-2 text-sm text-danger">
          {error}
        </div>
      ) : null}

      {!loading && !error && items.length === 0 ? (
        <div className="rounded-control px-3 py-2 text-sm text-text-muted">
          No matching commands
        </div>
      ) : null}

      {!error ? (
        <div className="max-h-[25vh] overflow-y-auto">
          {items.map((item, index) => {
            const Icon = KIND_ICON[item.kind];
            return (
              <button
                key={item.id}
                type="button"
                onMouseDown={(event) => {
                  event.preventDefault();
                  onSelect(item);
                }}
                className={cn(
                  'flex w-full items-start gap-3 rounded-control px-3 py-2 text-left hover:bg-surface-muted',
                  index === activeIndex && 'bg-surface-muted',
                )}
              >
                <Icon className="mt-0.5 size-4 shrink-0 text-text-muted" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-medium text-text">{item.label}</span>
                  <span className="mt-0.5 line-clamp-1 block text-xs text-text-muted">
                    {subtitle(item)}
                  </span>
                </span>
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
