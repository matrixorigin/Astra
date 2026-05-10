'use client';

import Link from 'next/link';
import { Avatar } from '@/components/ui/avatar';
import type { UserSummary } from '@/lib/api/types';

export function UserBadge({ user, collapsed }: { user: UserSummary; collapsed?: boolean }) {
  if (user.id === 'offline') {
    return (
      <Link
        href="/login?next=/"
        className="flex items-center gap-3 rounded-control border border-border bg-surface px-3 py-2 hover:bg-surface-muted"
      >
        <Avatar name="Sign in" />
        {collapsed ? null : (
          <div className="min-w-0 flex-1">
            <p className="truncate text-sm font-medium">Sign in</p>
            <p className="truncate text-xs text-text-muted">Connect Astra runtime</p>
          </div>
        )}
      </Link>
    );
  }

  return (
    <div className="flex items-center gap-3 rounded-control border border-border bg-surface px-3 py-2">
      <Avatar name={user.name} />
      {collapsed ? null : (
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium">{user.name}</p>
          <p className="truncate text-xs text-text-muted">{user.plan} plan</p>
        </div>
      )}
    </div>
  );
}
