'use client';

import Link from 'next/link';
import { usePathname } from 'next/navigation';
import {
  Box,
  ChevronLeft,
  ChevronRight,
  Folder,
  MessageSquare,
  MoreHorizontal,
  Plus,
  Search,
  Trash2,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import type { ReactNode } from 'react';
import { useCallback, useEffect, useState } from 'react';
import { ChatActionsMenu } from '@/components/app/chat-actions-menu';
import { UserBadge } from '@/components/app/user-badge';
import { IconButton } from '@/components/ui/icon-button';
import { SidebarSection } from '@/components/ui/sidebar-section';
import { useChatLifecycleActions } from '@/hooks/use-chat-lifecycle-actions';
import { useSidebar } from '@/hooks/use-sidebar';
import { subscribeChatLifecycleChange } from '@/lib/chat-lifecycle-events';
import { getSidebarData } from '@/lib/api/sidebar';
import type { RecentItem, SidebarData } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type NavItem = {
  href: string | null;
  label: string;
  icon: LucideIcon;
  disabled?: boolean;
  badge?: string;
};

const nav: NavItem[] = [
  { href: null, label: 'Search', icon: Search },
  { href: '/chats', label: 'Chats', icon: MessageSquare },
  { href: '/projects', label: 'Projects', icon: Folder },
];

const emptySidebar: SidebarData = {
  recents: [],
  recentProjectGroups: [],
  recentOtherChats: [],
  untitled: [],
  archivedChats: [],
  user: { id: 'offline', name: 'Astra user', plan: 'free' },
};

function activeFor(pathname: string, href: string) {
  return pathname === href || pathname.startsWith(`${href}/`);
}

export function Sidebar({ onSearch }: { onSearch: () => void }) {
  const pathname = usePathname();
  const { collapsed, toggle } = useSidebar();
  const [data, setData] = useState<SidebarData | null>(null);
  const lifecycle = useChatLifecycleActions();

  const reloadSidebar = useCallback(async () => {
    try {
      setData(await getSidebarData());
    } catch {
      setData(emptySidebar);
    }
  }, []);

  useEffect(() => {
    void reloadSidebar();
    const interval = window.setInterval(reloadSidebar, 60_000);
    window.addEventListener('focus', reloadSidebar);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener('focus', reloadSidebar);
    };
  }, [reloadSidebar]);

  useEffect(() => subscribeChatLifecycleChange(() => {
    void reloadSidebar();
  }), [reloadSidebar]);

  return (
    <aside
      className={cn(
        'hidden h-screen shrink-0 border-r border-border bg-surface-muted/70 p-3 md:flex md:flex-col',
        collapsed ? 'w-sidebar-collapsed' : 'w-sidebar',
      )}
    >
      <div className="flex items-center justify-between gap-2">
        <Link
          href="/"
          className="flex h-10 min-w-0 items-center gap-2 rounded-control px-2 text-sm font-semibold hover:bg-surface"
          aria-label="Astra home"
        >
          <span className="flex size-7 shrink-0 items-center justify-center rounded-control bg-text text-xs font-semibold text-white">
            A
          </span>
          {collapsed ? null : <span className="truncate">Astra</span>}
        </Link>
        <IconButton
          icon={collapsed ? ChevronRight : ChevronLeft}
          label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          onClick={toggle}
        />
      </div>

      <Link
        href="/"
        className={cn(
          'mt-4 flex h-10 items-center justify-center gap-2 rounded-control bg-text text-sm font-medium text-white hover:bg-text/90',
          collapsed && 'px-0',
        )}
      >
        <Plus className="size-4" />
        {collapsed ? null : <span>New chat</span>}
      </Link>

      <nav className="mt-4 space-y-1">
        {nav.map((item) => {
          if (!item.href) {
            return (
              <button
                key={item.label}
                type="button"
                disabled={item.disabled}
                onClick={item.disabled ? undefined : onSearch}
                className={cn(
                  'flex h-9 w-full items-center gap-3 rounded-control px-3 text-sm text-text-secondary hover:bg-surface hover:text-text disabled:cursor-not-allowed disabled:opacity-50',
                  collapsed && 'justify-center px-0',
                )}
              >
                <item.icon className="size-4 shrink-0" />
                {collapsed ? null : <span className="truncate">{item.label}</span>}
                {!collapsed && item.badge ? (
                  <span className="ml-auto rounded-full bg-surface px-2 py-0.5 text-xs text-text-muted">
                    {item.badge}
                  </span>
                ) : null}
              </button>
            );
          }
          return (
            <Link
              key={item.href}
              href={item.href}
              className={cn(
                'flex h-9 items-center gap-3 rounded-control px-3 text-sm text-text-secondary hover:bg-surface hover:text-text',
                activeFor(pathname, item.href) && 'bg-surface text-text',
                collapsed && 'justify-center px-0',
              )}
              aria-current={activeFor(pathname, item.href) ? 'page' : undefined}
            >
              <item.icon className="size-4 shrink-0" />
              {collapsed ? null : <span className="truncate">{item.label}</span>}
            </Link>
          );
        })}
      </nav>

      <div className="min-h-0 flex-1 overflow-y-auto pb-3">
        {collapsed ? null : (
          <>
            <SidebarSection label="Recents">
              {data ? (
                data.recentProjectGroups.length > 0 || data.recentOtherChats.length > 0 ? (
                  <div className="space-y-3">
                    {data.recentProjectGroups.length ? (
                      <SidebarRecentGroup label="Projects">
                        {data.recentProjectGroups.map((group) => (
                          <SidebarProjectGroup
                            key={group.project.id}
                            project={group.project}
                            chats={group.chats}
                            pathname={pathname}
                          />
                        ))}
                      </SidebarRecentGroup>
                    ) : null}
                    {data.recentOtherChats.length ? (
                      <SidebarRecentGroup label="Others">
                        {data.recentOtherChats.map((item) => (
                          <SidebarChatItem
                            key={`other-${item.id}`}
                            item={item}
                            active={pathname === item.href}
                          />
                        ))}
                      </SidebarRecentGroup>
                    ) : null}
                  </div>
                ) : (
                  <p className="px-3 py-2 text-sm text-text-muted">No chats yet</p>
                )
              ) : (
                Array.from({ length: 5 }).map((_, index) => (
                  <div key={index} className="h-8 rounded-control bg-surface" />
                ))
              )}
            </SidebarSection>

            {data?.untitled.length ? (
              <SidebarSection label="Untitled" defaultCollapsed>
                {data.untitled.map((item) => (
                  <SidebarChatItem
                    key={`untitled-${item.id}`}
                    item={item}
                    active={pathname === item.href}
                  />
                ))}
              </SidebarSection>
            ) : null}

            {data?.archivedChats.length ? (
              <SidebarSection label="Archived chats" defaultCollapsed>
                {data.archivedChats.map((item) => (
                  <SidebarChatItem
                    key={`archived-${item.id}`}
                    item={item}
                    archived
                    active={pathname === item.href}
                  />
                ))}
                <ClearArchivedButton
                  busy={lifecycle.clearingArchived}
                  onConfirm={async () => {
                    const deleted = await lifecycle.clearArchived({
                      redirectToChats: data.archivedChats.some((item) => pathname === item.href),
                    });
                    return deleted !== null;
                  }}
                />
              </SidebarSection>
            ) : null}
          </>
        )}
      </div>

      {data ? <UserBadge user={data.user} collapsed={collapsed} /> : <Box className="size-8 text-text-muted" />}
    </aside>
  );
}

function SidebarRecentGroup({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <div className="px-3 pb-1 text-[11px] font-medium uppercase text-text-muted">
        {label}
      </div>
      <div className="space-y-1">{children}</div>
    </div>
  );
}

function SidebarProjectGroup({
  project,
  chats,
  pathname,
}: {
  project: RecentItem;
  chats: RecentItem[];
  pathname: string;
}) {
  return (
    <div className="space-y-1">
      <Link
        href={project.href}
        className={cn(
          'flex h-8 min-w-0 items-center gap-3 rounded-control px-3 text-sm font-medium text-text-secondary hover:bg-surface hover:text-text',
          pathname === project.href && 'bg-surface text-text',
        )}
        aria-current={pathname === project.href ? 'page' : undefined}
      >
        <Folder className="size-4 shrink-0 text-text-muted" />
        <span className="truncate">{project.title}</span>
      </Link>
      <div className="space-y-1 pl-4">
        {chats.map((chat) => (
          <SidebarChatItem
            key={`${project.id}-${chat.id}`}
            item={chat}
            active={pathname === chat.href}
          />
        ))}
      </div>
    </div>
  );
}

function SidebarChatItem({
  item,
  active,
  archived = false,
}: {
  item: RecentItem;
  active: boolean;
  archived?: boolean;
}) {
  return (
    <div
      className={cn(
        'group flex min-w-0 items-center rounded-control text-sm text-text-secondary hover:bg-surface hover:text-text',
        active && 'bg-surface text-text',
      )}
    >
      <Link
        href={item.href}
        className="flex h-8 min-w-0 flex-1 items-center gap-3 px-3"
        aria-current={active ? 'page' : undefined}
      >
        <MessageSquare className="size-4 shrink-0 text-text-muted" />
        <span className="truncate">{item.title}</span>
      </Link>
      <ChatActionsMenu
        chatId={item.id}
        archived={archived}
        active={active}
        afterMutationHref={chatListHref(item.href)}
        variant="compact"
        trigger={(
          <button
            type="button"
            className="mr-1 flex size-7 shrink-0 items-center justify-center rounded-control text-text-muted opacity-0 hover:bg-surface-muted hover:text-text focus:opacity-100 group-hover:opacity-100"
            aria-label={`Open actions for ${item.title}`}
          >
            <MoreHorizontal className="size-4" />
          </button>
        )}
      />
    </div>
  );
}

function chatListHref(chatHref: string) {
  const match = chatHref.match(/^\/projects\/([^/]+)\/chats\//);
  return match ? `/projects/${match[1]}` : '/chats';
}

function ClearArchivedButton({
  busy,
  onConfirm,
}: {
  busy: boolean;
  onConfirm: () => Promise<boolean>;
}) {
  const [confirming, setConfirming] = useState(false);

  if (confirming) {
    return (
      <div className="mt-2 rounded-card border border-danger/20 bg-danger/5 p-3">
        <p className="text-xs leading-relaxed text-text-muted">
          Permanently delete every archived chat? This cannot be undone.
        </p>
        <div className="mt-2 flex justify-end gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => setConfirming(false)}
            className="rounded-control px-2 py-1 text-xs text-text-muted hover:bg-surface disabled:opacity-50"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={async () => {
              if (await onConfirm()) {
                setConfirming(false);
              }
            }}
            className="rounded-control bg-danger px-2 py-1 text-xs font-medium text-white hover:bg-danger/90 disabled:opacity-50"
          >
            Clear
          </button>
        </div>
      </div>
    );
  }

  return (
    <button
      type="button"
      onClick={() => setConfirming(true)}
      className="mt-2 flex w-full items-center gap-2 rounded-control px-3 py-2 text-sm text-danger hover:bg-danger/10"
    >
      <Trash2 className="size-4" />
      <span>Clear archived</span>
    </button>
  );
}
