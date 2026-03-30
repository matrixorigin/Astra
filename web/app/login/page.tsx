'use client';

import Link from 'next/link';
import { useRouter, useSearchParams } from 'next/navigation';
import { Suspense, useActionState } from 'react';
import { loginAction } from '@/lib/auth/actions';

function LoginForm() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const next = searchParams.get('next') || '/overview';
  const [state, formAction, isPending] = useActionState(loginAction, { ok: false });

  if (state.ok) {
    router.push(next);
    return null;
  }

  return (
    <>
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
            autoComplete="username"
            className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
            placeholder="your-username"
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
            autoComplete="current-password"
            className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50"
            placeholder="••••••••"
          />
        </div>

        <button
          type="submit"
          disabled={isPending}
          className="w-full rounded-xl bg-sky-600 py-3 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
        >
          {isPending ? 'Signing in…' : 'Sign in'}
        </button>
      </form>

      <p className="mt-6 text-center text-sm text-slate-400">
        Don&apos;t have an account?{' '}
        <Link href="/register" className="text-sky-400 hover:text-sky-300">
          Register
        </Link>
      </p>

      <p className="mt-2 text-center text-sm text-slate-500">
        <Link href="/settings" className="hover:text-slate-300">
          Configure API URL →
        </Link>
      </p>
    </>
  );
}

export default function LoginPage() {
  return (
    <div className="flex min-h-screen items-center justify-center bg-slate-950 p-4">
      <div className="w-full max-w-sm">
        <div className="mb-8 text-center">
          <h1 className="text-2xl font-bold text-white">mo-dev-agent</h1>
          <p className="mt-2 text-sm text-slate-400">Sign in to your account</p>
        </div>
        <Suspense fallback={<div className="h-64 animate-pulse rounded-xl bg-slate-800/50" />}>
          <LoginForm />
        </Suspense>
      </div>
    </div>
  );
}
