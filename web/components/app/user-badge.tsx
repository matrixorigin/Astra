'use client';

import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { LogOut, UserRound } from 'lucide-react';
import { useState } from 'react';
import { Avatar } from '@/components/ui/avatar';
import { Menu, MenuItem } from '@/components/ui/menu';
import type { UserSummary } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

export function UserBadge({ user, collapsed }: { user: UserSummary; collapsed?: boolean }) {
  const router = useRouter();
  const [signingOut, setSigningOut] = useState(false);
  const [error, setError] = useState<string | null>(null);

  if (user.id === 'offline') {
    return (
      <Link
        href="/login?next=/"
        className="astra-sidebar-account flex items-center gap-3 rounded-control px-3 py-2"
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

  async function signOut() {
    setSigningOut(true);
    setError(null);
    try {
      const response = await fetch('/api/runtime-auth/logout', {
        method: 'POST',
        cache: 'no-store',
      });

      if (!response.ok) {
        const body = (await response.json().catch(() => ({}))) as { error?: string };
        throw new Error(body.error ?? `Sign out failed: ${response.status}`);
      }

      router.push('/');
      router.refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Sign out failed.');
    } finally {
      setSigningOut(false);
    }
  }

  const trigger = (
    <button
      type="button"
      className={cn(
        'astra-sidebar-account flex w-full items-center gap-3 rounded-control px-3 py-2 text-left',
        collapsed && 'justify-center px-0',
      )}
      aria-label="Account menu"
    >
      <Avatar name={user.name} />
      {collapsed ? null : (
        <div className="min-w-0 flex-1">
          <p className="truncate text-sm font-medium">{user.name}</p>
          <p className="truncate text-xs text-text-muted">{user.plan} plan</p>
        </div>
      )}
    </button>
  );

  return (
    <Menu trigger={trigger}>
      <div className="border-b border-border px-3 py-2">
        <div className="flex items-center gap-2">
          <UserRound className="size-4 text-text-muted" />
          <div className="min-w-0">
            <p className="truncate text-sm font-medium">{user.name}</p>
            <p className="truncate text-xs text-text-muted">{user.plan} plan</p>
          </div>
        </div>
      </div>
      <MenuItem
        icon={LogOut}
        disabled={signingOut}
        onSelect={(event) => {
          event.preventDefault();
          void signOut();
        }}
      >
        {signingOut ? 'Signing out...' : 'Sign out'}
      </MenuItem>
      {error ? (
        <p className="px-3 py-2 text-xs leading-relaxed text-danger">{error}</p>
      ) : null}
    </Menu>
  );
}
