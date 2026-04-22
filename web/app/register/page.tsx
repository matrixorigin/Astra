'use client';

import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useActionState, useEffect } from 'react';
import { registerAction } from '@/lib/auth/actions';

export default function RegisterPage() {
  const router = useRouter();
  const [state, formAction, isPending] = useActionState(registerAction, { ok: false });

  useEffect(() => {
    if (state.ok) {
      router.push('/overview');
    }
  }, [state.ok, router]);

  if (state.ok) {
    return (
      <div className="flex min-h-screen items-center justify-center bg-slate-950 p-4">
        <div className="w-full max-w-sm">
          <p className="text-center text-sm text-slate-400" role="status">
            Account created. Redirecting…
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-slate-950 p-4">
      <div className="w-full max-w-sm">
        <div className="mb-8 text-center">
          <h1 className="text-2xl font-bold text-white">mo-dev-agent</h1>
          <p className="mt-2 text-sm text-slate-400">Create a new account</p>
        </div>

        <form action={formAction} className="space-y-4">
          {state.error ? (
            <div className="rounded-xl border border-red-800/50 bg-red-950/30 px-4 py-3 text-sm text-red-300">
              {state.error}
            </div>
          ) : null}

          <div>
            <label htmlFor="username" className="block text-sm font-medium text-slate-300">
              Username
            </label>
            <input
              id="username"
              name="username"
              type="text"
              required
              minLength={3}
              maxLength={50}
              autoComplete="username"
              className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
              placeholder="3-50 chars, alphanumeric + _ -"
            />
          </div>

          <div>
            <label htmlFor="email" className="block text-sm font-medium text-slate-300">
              Email
            </label>
            <input
              id="email"
              name="email"
              type="email"
              required
              autoComplete="email"
              className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
              placeholder="you@example.com"
            />
          </div>

          <div>
            <label htmlFor="password" className="block text-sm font-medium text-slate-300">
              Password
            </label>
            <input
              id="password"
              name="password"
              type="password"
              required
              minLength={8}
              maxLength={72}
              autoComplete="new-password"
              className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
              placeholder="8-72 characters"
            />
          </div>

          <div>
            <label htmlFor="display_name" className="block text-sm font-medium text-slate-300">
              Display name <span className="text-slate-500">(optional)</span>
            </label>
            <input
              id="display_name"
              name="display_name"
              type="text"
              maxLength={255}
              autoComplete="name"
              className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
              placeholder="Your name"
            />
          </div>

          <button
            type="submit"
            disabled={isPending}
            className="w-full rounded-xl bg-sky-600 py-3 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
          >
            {isPending ? 'Creating account…' : 'Create account'}
          </button>
        </form>

        <p className="mt-6 text-center text-sm text-slate-400">
          Already have an account?{' '}
          <Link href="/login" className="text-sky-400 hover:text-sky-300">
            Sign in
          </Link>
        </p>
      </div>
    </div>
  );
}
