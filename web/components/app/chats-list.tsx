'use client';

import { MessageSquare } from 'lucide-react';
import { useCallback, useEffect, useState } from 'react';
import { Button } from '@/components/ui/button';
import { EmptyState } from '@/components/ui/empty-state';
import { PageHeader } from '@/components/ui/page-header';
import { SearchField } from '@/components/ui/search-field';
import { ChatRow } from '@/components/app/chat-row';
import { listChats } from '@/lib/api/chats';
import type { ChatSummary } from '@/lib/api/types';
import { relativeTime } from '@/lib/utils/time';
import { useDebouncedValue } from '@/hooks/use-debounced-value';
import { subscribeChatLifecycleChange } from '@/lib/chat-lifecycle-events';

export function ChatsList({ projectId = null }: { projectId?: string | null }) {
  const [query, setQuery] = useState('');
  const debounced = useDebouncedValue(query, 250);
  const [items, setItems] = useState<ChatSummary[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    listChats({ projectId, q: debounced, limit: 50 })
      .then((result) => {
        if (!cancelled) {
          setItems(result.items);
          setNextCursor(result.nextCursor);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : 'Failed to load chats');
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [debounced, projectId]);

  useEffect(() => reload(), [reload]);

  useEffect(() => subscribeChatLifecycleChange(() => {
    void reload();
  }), [reload]);

  async function loadMore() {
    if (!nextCursor) {
      return;
    }
    const result = await listChats({ projectId, q: debounced, cursor: nextCursor, limit: 50 });
    setItems((current) => [...current, ...result.items]);
    setNextCursor(result.nextCursor);
  }

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto w-full max-w-5xl">
        <PageHeader
          title={projectId ? 'Project chats' : 'Chats'}
          description={
            projectId
              ? 'Conversations grounded in this project workspace.'
              : 'Continue durable sessions or start a new line of work.'
          }
          action={<Button href="/">New chat</Button>}
        />
        <SearchField
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search chats..."
          containerClassName="mt-6"
        />

      {error ? (
        <div className="mt-6 rounded-card border border-danger/20 bg-danger/5 px-4 py-3 text-sm text-danger">
          {error}
        </div>
      ) : null}

      <div className="mt-6 space-y-2">
        {loading
          ? Array.from({ length: 10 }).map((_, index) => (
              <div key={index} className="h-16 rounded-card border border-border bg-surface" />
            ))
          : items.map((chat) => (
              <ChatRow
                key={chat.id}
                chatId={chat.id}
                title={chat.title || 'Untitled'}
                subtitle={`Last message ${relativeTime(chat.lastMessageAt)}`}
                href={chat.projectId ? `/projects/${chat.projectId}/chats/${chat.id}` : `/chats/${chat.id}`}
                archived={Boolean(chat.archivedAt)}
                afterMutationHref={chat.projectId ? `/projects/${chat.projectId}` : '/chats'}
              />
            ))}
      </div>

      {!loading && items.length === 0 ? (
        <div className="mt-8">
          <EmptyState
            icon={query ? MessageSquare : MessageSquare}
            title={query ? `No results for "${query}"` : 'No chats yet'}
            description={query ? undefined : 'Start a new conversation to see it here.'}
            cta={!query ? <Button href="/">New chat</Button> : null}
          />
        </div>
      ) : null}

      {nextCursor ? (
        <div className="mt-6 flex justify-center">
          <Button variant="ghost" onClick={loadMore}>
            Load more
          </Button>
        </div>
      ) : null}
      </div>
    </div>
  );
}
