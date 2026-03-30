'use client';

import Link from 'next/link';
import { usePathname } from 'next/navigation';
import clsx from 'clsx';

export function NavLink({
  href,
  children,
}: {
  href: string;
  children: React.ReactNode;
}) {
  const pathname = usePathname();
  const active = pathname === href;

  return (
    <Link
      href={href}
      className={clsx(
        'flex items-center rounded-2xl px-4 py-3 text-sm transition',
        active
          ? 'bg-sky-400/10 text-sky-300 ring-1 ring-sky-400/30'
          : 'text-slate-300 hover:bg-slate-900 hover:text-white',
      )}
    >
      {children}
    </Link>
  );
}
