'use client';

import Link from 'next/link';
import { useRouter } from 'next/navigation';
import { useActionState, useEffect } from 'react';
import { AuthShell } from '@/components/auth/auth-shell';
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
      <AuthShell
        eyebrow="Account ready"
        title="Opening your workspace."
        description="Your account was created successfully."
        footer={null}
      >
        <div
          className="rounded-card border border-success/20 bg-success/5 px-4 py-3 text-sm text-success"
          role="status"
        >
          Account created. Preparing Astra…
        </div>
      </AuthShell>
    );
  }

  return (
    <AuthShell
      eyebrow="Create account"
      title="Start with a durable workspace."
      description="Create an identity for sessions that can be resumed, inspected, delegated, and integrated."
      footer={
        <>
          Already have an account?{' '}
          <Link href="/login" className="font-medium text-text hover:text-accent">
            Sign in
          </Link>
        </>
      }
    >
        <form action={formAction} className="space-y-4">
          {state.error ? (
            <div
              role="alert"
              className="rounded-card border border-danger/20 bg-danger/5 px-4 py-3 text-sm leading-5 text-danger"
            >
              {state.error}
            </div>
          ) : null}

          <div>
            <label htmlFor="username" className="block text-sm font-medium text-text">
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
              autoFocus
              className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
              placeholder="3–50 characters"
            />
          </div>

          <div>
            <label htmlFor="email" className="block text-sm font-medium text-text">
              Email
            </label>
            <input
              id="email"
              name="email"
              type="email"
              required
              autoComplete="email"
              className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
              placeholder="you@example.com"
            />
          </div>

          <div>
            <label htmlFor="password" className="block text-sm font-medium text-text">
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
              className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
              placeholder="8-72 characters"
            />
          </div>

          <div>
            <label htmlFor="display_name" className="block text-sm font-medium text-text">
              Display name <span className="font-normal text-text-muted">(optional)</span>
            </label>
            <input
              id="display_name"
              name="display_name"
              type="text"
              maxLength={255}
              autoComplete="name"
              className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
              placeholder="Your name"
            />
          </div>

          <button
            type="submit"
            disabled={isPending}
            className="inline-flex h-11 w-full items-center justify-center rounded-control bg-text px-4 text-sm font-semibold text-white transition hover:bg-text/90 disabled:cursor-wait disabled:opacity-60"
          >
            {isPending ? 'Creating account…' : 'Create account'}
          </button>
        </form>
    </AuthShell>
  );
}
