import Link from 'next/link';
import { getCurrentUser } from '@/lib/auth/actions';
import { LogoutButton } from './logout-button';

export async function UserBadge() {
  const user = await getCurrentUser();

  if (!user) {
    return (
      <Link
        href="/login"
        className="block rounded-xl border border-slate-700 px-4 py-3 text-center text-sm text-slate-300 hover:border-sky-500/50 hover:text-white"
      >
        Sign in
      </Link>
    );
  }

  return (
    <div className="flex items-center gap-3">
      <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-sky-600/20 text-xs font-bold text-sky-300">
        {user.username.charAt(0).toUpperCase()}
      </div>
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-white">
          {user.display_name ?? user.username}
        </p>
        <p className="truncate text-xs text-slate-500">{user.email}</p>
      </div>
      <LogoutButton />
    </div>
  );
}
