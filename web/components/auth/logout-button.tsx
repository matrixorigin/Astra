'use client';

import { logoutAction } from '@/lib/auth/actions';

export function LogoutButton() {
  return (
    <form action={logoutAction}>
      <button
        type="submit"
        className="shrink-0 text-xs text-slate-400 hover:text-red-300"
        title="Sign out"
      >
        ↗
      </button>
    </form>
  );
}
