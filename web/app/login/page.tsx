"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Suspense, useActionState, useEffect } from "react";
import { AuthShell } from "@/components/auth/auth-shell";
import { loginAction } from "@/lib/auth/actions";

function LoginForm() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const next = searchParams.get("next") || "/overview";
  const [state, formAction, isPending] = useActionState(loginAction, {
    ok: false,
  });

  useEffect(() => {
    if (state.ok) {
      router.push(next);
    }
  }, [state.ok, next, router]);

  if (state.ok) {
    return (
      <div
        className="rounded-card border border-success/20 bg-success/5 px-4 py-3 text-sm text-success"
        role="status"
      >
        Signed in. Opening your workspace…
      </div>
    );
  }

  return (
    <form action={formAction} className="space-y-5">
        {state.error ? (
          <div
            role="alert"
            className="rounded-card border border-danger/20 bg-danger/5 px-4 py-3 text-sm leading-5 text-danger"
          >
            {state.error}
          </div>
        ) : null}

        <div>
          <label
            htmlFor="username"
            className="block text-sm font-medium text-text"
          >
            Username
          </label>
          <input
            id="username"
            name="username"
            type="text"
            required
            autoComplete="username"
            autoFocus
            className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
            placeholder="Your username"
          />
        </div>

        <div>
          <label
            htmlFor="password"
            className="block text-sm font-medium text-text"
          >
            Password
          </label>
          <input
            id="password"
            name="password"
            type="password"
            required
            autoComplete="current-password"
            className="mt-2 h-11 w-full rounded-control border border-border bg-surface px-3.5 text-sm text-text outline-none transition placeholder:text-text-muted focus:border-accent focus:ring-4 focus:ring-accent/10"
            placeholder="••••••••"
          />
        </div>

        <button
          type="submit"
          disabled={isPending}
          className="inline-flex h-11 w-full items-center justify-center rounded-control bg-text px-4 text-sm font-semibold text-white transition hover:bg-text/90 disabled:cursor-wait disabled:opacity-60"
        >
          {isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>
  );
}

export default function LoginPage() {
  return (
    <AuthShell
      eyebrow="Welcome back"
      title="Continue your agent workspace."
      description="Return to durable sessions, active tasks, delegated runs, and the evidence behind each decision."
      footer={
        <>
          New to Astra?{" "}
          <Link href="/register" className="font-medium text-text hover:text-accent">
            Create an account
          </Link>
        </>
      }
    >
      <Suspense
        fallback={
          <div className="h-64 animate-pulse rounded-card bg-surface-muted" />
        }
      >
        <LoginForm />
      </Suspense>
    </AuthShell>
  );
}
