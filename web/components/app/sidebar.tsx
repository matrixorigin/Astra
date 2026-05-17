'use client';

import Link from 'next/link';
import { usePathname } from 'next/navigation';
import {
  Box,
  ChevronLeft,
  ChevronRight,
  MoreHorizontal,
  Trash2,
} from 'lucide-react';
import type { ReactNode } from 'react';
import { useCallback, useEffect, useState } from 'react';
import { ChatActionsMenu } from '@/components/app/chat-actions-menu';
import { TuiEntityMark } from '@/components/app/tui-entity-mark';
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
  mark: 'chat' | 'project' | 'search';
  disabled?: boolean;
  badge?: string;
};

const nav: NavItem[] = [
  { href: null, label: 'Search', mark: 'search' },
  { href: '/chats', label: 'Chats', mark: 'chat' },
  { href: '/projects', label: 'Projects', mark: 'project' },
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
        'astra-sidebar hidden h-screen shrink-0 p-3 md:flex md:flex-col',
        collapsed ? 'w-sidebar-collapsed' : 'w-sidebar',
      )}
    >
      <div className="astra-sidebar-header flex items-center justify-between gap-2">
        <Link
          href="/"
          className="astra-sidebar-brand flex h-10 min-w-0 items-center gap-2 rounded-control px-2 text-sm font-semibold"
          aria-label="Astra home"
        >
          <span className="astra-sidebar-brand-mark flex size-7 shrink-0 items-center justify-center rounded-control text-xs font-semibold">
            A
          </span>
          {collapsed ? null : (
            <span className="min-w-0">
              <span className="block truncate">Astra</span>
              <span className="astra-sidebar-brand-subtitle block truncate">agent shell</span>
            </span>
          )}
        </Link>
        <IconButton
          icon={collapsed ? ChevronRight : ChevronLeft}
          label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
          onClick={toggle}
          className="astra-sidebar-icon-button"
        />
      </div>

      <Link
        href="/"
        className={cn(
          'astra-sidebar-command mt-4 flex h-10 items-center justify-center gap-2 rounded-control text-sm font-medium',
          collapsed && 'px-0',
        )}
      >
        <TuiEntityMark kind="new" className="astra-sidebar-command-mark" />
        {collapsed ? null : <span>New chat</span>}
      </Link>

      <nav className="astra-sidebar-nav mt-4 space-y-1" aria-label="Primary">
        {nav.map((item) => {
          if (!item.href) {
            return (
              <button
                key={item.label}
                type="button"
                disabled={item.disabled}
                onClick={item.disabled ? undefined : onSearch}
                className={cn(
                  'astra-sidebar-nav-item flex h-9 w-full items-center gap-3 rounded-control px-3 text-sm disabled:cursor-not-allowed disabled:opacity-50',
                  collapsed && 'justify-center px-0',
                )}
              >
                <TuiEntityMark kind={item.mark} className="astra-sidebar-mark" />
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
                'astra-sidebar-nav-item flex h-9 items-center gap-3 rounded-control px-3 text-sm',
                activeFor(pathname, item.href) && 'is-active',
                collapsed && 'justify-center px-0',
              )}
              aria-current={activeFor(pathname, item.href) ? 'page' : undefined}
            >
              <TuiEntityMark kind={item.mark} className="astra-sidebar-mark" />
              {collapsed ? null : <span className="truncate">{item.label}</span>}
            </Link>
          );
        })}
      </nav>

      <div className="astra-sidebar-scroll min-h-0 flex-1 overflow-y-auto pb-3">
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
                  <div key={index} className="astra-sidebar-skeleton h-8 rounded-control" />
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
    <div className="astra-sidebar-recent-group">
      <div className="astra-sidebar-group-label px-3 pb-1 text-[11px] font-medium uppercase">
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
          'astra-sidebar-list-item flex h-8 min-w-0 items-center gap-3 rounded-control px-3 text-sm font-medium',
          pathname === project.href && 'is-active',
        )}
        aria-current={pathname === project.href ? 'page' : undefined}
      >
        <TuiEntityMark kind="project" className="astra-sidebar-mark" />
        <span className="truncate">{project.title}</span>
      </Link>
      <div className="astra-sidebar-project-chats space-y-1">
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
        'astra-sidebar-list-item group flex min-w-0 items-center rounded-control text-sm',
        active && 'is-active',
      )}
    >
      <Link
        href={item.href}
        className="flex h-8 min-w-0 flex-1 items-center gap-3 px-3"
        aria-current={active ? 'page' : undefined}
      >
        <TuiEntityMark kind="chat" className="astra-sidebar-mark" />
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
            className="astra-sidebar-item-action mr-1 flex size-7 shrink-0 items-center justify-center rounded-control opacity-0 focus:opacity-100 group-hover:opacity-100"
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
      <div className="astra-sidebar-confirm mt-2 rounded-card p-3">
        <p className="text-xs leading-relaxed text-text-muted">
          Permanently delete every archived chat? This cannot be undone.
        </p>
        <div className="mt-2 flex justify-end gap-2">
          <button
            type="button"
            disabled={busy}
            onClick={() => setConfirming(false)}
            className="astra-sidebar-confirm-secondary rounded-control px-2 py-1 text-xs disabled:opacity-50"
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
            className="astra-sidebar-confirm-danger rounded-control px-2 py-1 text-xs font-medium disabled:opacity-50"
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
      className="astra-sidebar-clear mt-2 flex w-full items-center gap-2 rounded-control px-3 py-2 text-sm"
    >
      <Trash2 className="size-4" />
      <span>Clear archived</span>
    </button>
  );
}
