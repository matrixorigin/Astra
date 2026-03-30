import Link from 'next/link';
import { NavLink } from '@/components/nav-link';
import { UserBadge } from '@/components/auth/user-badge';

const navigation = [
  { href: '/overview', label: 'Overview' },
  { href: '/agents', label: 'Agents' },
  { href: '/sessions', label: 'Sessions' },
  { href: '/runs', label: 'Runs' },
  { href: '/events', label: 'Events' },
  { href: '/plans', label: 'Plans' },
  { href: '/introspection', label: 'Introspection' },
  { href: '/evaluation', label: 'Evaluation' },
  { href: '/workspace', label: 'Workspace' },
  { href: '/settings', label: 'Settings' },
];

export function AppShell({ children }: { children: React.ReactNode }) {
  return (
    <div className="min-h-screen bg-slate-950 text-slate-100">
      <div className="mx-auto flex min-h-screen max-w-[1600px] flex-col lg:flex-row">
        <aside className="border-b border-slate-800 bg-slate-950/95 lg:min-h-screen lg:w-72 lg:border-b-0 lg:border-r">
          <div className="sticky top-0 p-6">
            <Link href="/overview" className="block">
              <p className="text-sm font-semibold uppercase tracking-[0.24em] text-sky-300">
                mo-agent web
              </p>
              <h1 className="mt-2 text-2xl font-semibold text-white">Platform console</h1>
            </Link>

            <p className="mt-4 text-sm leading-6 text-slate-400">
              A standalone frontend for platform visibility now, and browser-native agent
              interaction later.
            </p>

            <nav className="mt-8 space-y-2">
              {navigation.map((item) => (
                <NavLink key={item.href} href={item.href}>
                  {item.label}
                </NavLink>
              ))}
            </nav>

            <div className="mt-8 border-t border-slate-800 pt-4">
              <UserBadge />
            </div>
          </div>
        </aside>

        <main className="flex-1 p-6 lg:p-10">
          {children}
        </main>
      </div>
    </div>
  );
}
