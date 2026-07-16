'use client';

import Link from 'next/link';
import { CheckSquare, MoreHorizontal, Square } from 'lucide-react';
import { ChatActionsMenu } from '@/components/app/chat-actions-menu';
import { TuiEntityMark } from '@/components/app/tui-entity-mark';
import { cn } from '@/lib/utils/cn';

export function ChatRow({
  chatId,
  title,
  subtitle,
  href,
  archived,
  afterMutationHref,
  selectable,
  selected,
  onSelectChange,
}: {
  chatId: string;
  title: string;
  subtitle: string;
  href: string;
  archived?: boolean;
  afterMutationHref?: string;
  selectable?: boolean;
  selected?: boolean;
  onSelectChange?: (selected: boolean) => void;
}) {
  const content = (
    <>
      <TuiEntityMark kind="chat" className="mt-0.5" />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium">{title}</span>
        <span className="block truncate text-xs text-text-muted">{subtitle}</span>
      </span>
    </>
  );

  if (selectable) {
    return (
      <button
        type="button"
        onClick={() => onSelectChange?.(!selected)}
        className={cn(
          'flex w-full items-start gap-3 rounded-control border border-transparent px-3 py-3 text-left text-text-secondary hover:border-border hover:bg-surface-muted hover:text-text',
          selected && 'border-accent/30 bg-accent-soft text-accent',
        )}
      >
        {content}
        <span className="text-text-muted">
          {selected ? <CheckSquare className="size-4" /> : <Square className="size-4" />}
        </span>
      </button>
    );
  }

  return (
    <div className="group flex items-start gap-2 rounded-card border border-border/75 bg-surface pr-1 text-text-secondary shadow-[0_1px_2px_rgba(15,23,42,0.02)] transition hover:border-border-strong hover:text-text hover:shadow-[0_6px_18px_rgba(15,23,42,0.05)]">
      <Link href={href} className="flex min-w-0 flex-1 items-start gap-3 px-3 py-3">
        {content}
      </Link>
      <ChatActionsMenu
        chatId={chatId}
        archived={archived}
        afterMutationHref={afterMutationHref}
        variant="compact"
        trigger={(
          <button
            type="button"
            aria-label={`Open actions for ${title}`}
            className="mt-2 flex size-8 shrink-0 items-center justify-center rounded-control text-text-muted hover:bg-surface hover:text-text"
          >
            <MoreHorizontal className="size-4" />
          </button>
        )}
      />
    </div>
  );
}
