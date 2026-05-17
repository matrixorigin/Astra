'use client';

import { Command } from 'cmdk';
import { useRouter } from 'next/navigation';
import { useEffect, useMemo, useState } from 'react';
import { TuiEntityMark } from '@/components/app/tui-entity-mark';
import { Modal } from '@/components/ui/modal';
import { searchWorkspace } from '@/lib/api/search';
import type { SearchResponse } from '@/lib/api/types';
import { compactRelativeTime } from '@/lib/utils/time';
import { useDebouncedValue } from '@/hooks/use-debounced-value';

export function SearchModal({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const router = useRouter();
  const [query, setQuery] = useState('');
  const debounced = useDebouncedValue(query, 250);
  const [results, setResults] = useState<SearchResponse>({ projects: [], chats: [] });
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    if (!open) {
      return;
    }
    let cancelled = false;
    setLoading(true);
    searchWorkspace(debounced)
      .then((value) => {
        if (!cancelled) {
          setResults(value);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setResults({ projects: [], chats: [] });
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
  }, [debounced, open]);

  const total = results.projects.length + results.chats.length;
  const emptyText = useMemo(() => {
    if (loading) {
      return 'Searching...';
    }
    return query.trim() ? `No results for "${query.trim()}"` : 'No recent chats or projects';
  }, [loading, query]);

  function go(href: string) {
    onOpenChange(false);
    router.push(href);
  }

  return (
    <Modal open={open} onOpenChange={onOpenChange} title="Search workspace" titleHidden width={720}>
      <Command className="bg-surface p-3">
        <Command.Input
          autoFocus
          value={query}
          onValueChange={setQuery}
          placeholder="Search chats and projects"
          className="h-12 w-full rounded-control border border-border bg-surface px-4 text-sm outline-none focus:border-accent"
        />
        <Command.List className="mt-2 max-h-[420px] overflow-y-auto">
          <Command.Empty className="px-3 py-8 text-center text-sm text-text-muted">
            {emptyText}
          </Command.Empty>

          {results.projects.length ? (
            <Command.Group heading="Projects" className="py-2 text-xs text-text-muted">
              {results.projects.map((project) => (
                <Command.Item
                  key={`project-${project.id}`}
                  value={`project ${project.name}`}
                  onSelect={() => go(`/projects/${project.id}`)}
                  className="flex cursor-default items-center gap-3 rounded-control px-3 py-2 text-sm text-text outline-none aria-selected:bg-surface-muted"
                >
                  <TuiEntityMark kind="project" />
                  <span className="min-w-0 flex-1 truncate">{project.name}</span>
                  <span className="text-xs text-text-muted">{compactRelativeTime(project.updatedAt)}</span>
                </Command.Item>
              ))}
            </Command.Group>
          ) : null}

          {results.chats.length ? (
            <Command.Group heading="Chats" className="py-2 text-xs text-text-muted">
              {results.chats.map((chat) => (
                <Command.Item
                  key={`chat-${chat.id}`}
                  value={`chat ${chat.title ?? 'Untitled'}`}
                  onSelect={() => go(chat.projectId ? `/projects/${chat.projectId}/chats/${chat.id}` : `/chats/${chat.id}`)}
                  className="flex cursor-default items-center gap-3 rounded-control px-3 py-2 text-sm text-text outline-none aria-selected:bg-surface-muted"
                >
                  <TuiEntityMark kind="chat" />
                  <span className="min-w-0 flex-1 truncate">{chat.title ?? 'Untitled'}</span>
                  <span className="text-xs text-text-muted">{compactRelativeTime(chat.updatedAt)}</span>
                </Command.Item>
              ))}
            </Command.Group>
          ) : null}

          {!loading && total === 0 ? null : null}
        </Command.List>
      </Command>
    </Modal>
  );
}
