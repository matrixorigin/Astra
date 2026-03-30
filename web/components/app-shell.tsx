import Link from 'next/link';
import { NavLink } from '@/components/nav-link';

const navigation = [
  { href: '/overview', label: 'Overview' },
  { href: '/agents', label: 'Agents' },
  { href: '/sessions', label: 'Sessions' },
  { href: '/runs', label: 'Runs' },
  { href: '/events', label: 'Events' },
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
          </div>
        </aside>

        <main className="flex-1 p-6 lg:p-10">
          <header className="mb-8 flex flex-wrap items-center justify-between gap-4">
            <div>
              <p className="text-sm uppercase tracking-[0.2em] text-slate-500">
                agent runtime frontend
              </p>
              <p className="mt-2 text-sm text-slate-400">
                Initial scaffold with mock platform state, ready to be wired to real APIs.
              </p>
            </div>

            <div className="rounded-full border border-slate-800 bg-slate-900/80 px-4 py-2 text-sm text-slate-300">
              Status: scaffolded
            </div>
          </header>

          {children}
        </main>
      </div>
    </div>
  );
}
