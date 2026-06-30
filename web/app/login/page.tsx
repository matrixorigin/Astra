"use client";

import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Suspense, useActionState, useEffect, useState } from "react";
import { externalProvidersAction, loginAction } from "@/lib/auth/actions";
import type { ExternalAuthProvider } from "@/lib/auth/runtime-auth-client";

function LoginForm() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const next = searchParams.get("next") || "/overview";
  const [state, formAction, isPending] = useActionState(loginAction, {
    ok: false,
  });
  const [mode, setMode] = useState<"internal" | "external">("internal");
  const [providers, setProviders] = useState<ExternalAuthProvider[]>([]);
  const [providerId, setProviderId] = useState("");
  const [providerError, setProviderError] = useState<string | null>(null);
  const [providersLoading, setProvidersLoading] = useState(false);
  const [providersLoaded, setProvidersLoaded] = useState(false);

  useEffect(() => {
    if (state.ok) {
      router.push(next);
    }
  }, [state.ok, next, router]);

  useEffect(() => {
    if (mode !== "external") {
      setProvidersLoaded(false);
      return;
    }
    if (providersLoaded || providersLoading) {
      return;
    }
    setProvidersLoading(true);
    setProviderError(null);
    externalProvidersAction()
      .then((result) => {
        if (!result.ok) {
          setProviders([]);
          setProviderId("");
          setProviderError(result.error);
          return;
        }
        setProviders(result.providers);
        setProviderId(result.providers[0]?.id ?? "");
        setProviderError(
          result.providers.length === 0
            ? "No external providers are configured."
            : null,
        );
      })
      .catch((error: unknown) => {
        setProviders([]);
        setProviderId("");
        setProviderError(
          error instanceof Error
            ? error.message
            : "Failed to load external providers.",
        );
      })
      .finally(() => {
        setProvidersLoaded(true);
        setProvidersLoading(false);
      });
  }, [mode, providersLoaded, providersLoading]);

  if (state.ok) {
    return (
      <p className="text-center text-sm text-slate-400" role="status">
        Signed in. Redirecting…
      </p>
    );
  }

  return (
    <>
      <form action={formAction} className="space-y-4">
        <input type="hidden" name="auth_mode" value={mode} />
        {state.error ? (
          <div className="rounded-xl border border-red-800/50 bg-red-950/30 px-4 py-3 text-sm text-red-300">
            {state.error}
          </div>
        ) : null}

        <div className="grid grid-cols-2 rounded-xl border border-slate-700 bg-slate-900/50 p-1">
          <button
            type="button"
            onClick={() => setMode("internal")}
            className={`rounded-lg px-3 py-2 text-sm font-medium ${
              mode === "internal"
                ? "bg-sky-600 text-white"
                : "text-slate-400 hover:text-slate-200"
            }`}
          >
            Astra user
          </button>
          <button
            type="button"
            onClick={() => setMode("external")}
            className={`rounded-lg px-3 py-2 text-sm font-medium ${
              mode === "external"
                ? "bg-sky-600 text-white"
                : "text-slate-400 hover:text-slate-200"
            }`}
          >
            External user
          </button>
        </div>

        {mode === "external" ? (
          <div>
            <label
              htmlFor="provider_id"
              className="block text-sm font-medium text-slate-300"
            >
              Provider
            </label>
            <select
              id="provider_id"
              name="provider_id"
              required
              value={providerId}
              onChange={(event) => setProviderId(event.target.value)}
              disabled={providersLoading || providers.length === 0}
              className="mt-1 w-full rounded-xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none focus:border-sky-500/50 disabled:opacity-50"
            >
              {providers.map((provider) => (
                <option key={provider.id} value={provider.id}>
                  {provider.display_name}
                </option>
              ))}
            </select>
            {providerError ? (
              <p className="mt-2 text-sm text-red-300">{providerError}</p>
            ) : null}
          </div>
        ) : null}

        <div>
          <label
            htmlFor="username"
            className="block text-sm font-medium text-slate-300"
          >
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
          <label
            htmlFor="password"
            className="block text-sm font-medium text-slate-300"
          >
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
          disabled={
            isPending ||
            (mode === "external" && (providersLoading || !providerId))
          }
          className="w-full rounded-xl bg-sky-600 py-3 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-50"
        >
          {isPending ? "Signing in…" : "Sign in"}
        </button>
      </form>

      <p className="mt-6 text-center text-sm text-slate-400">
        Don&apos;t have an account?{" "}
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
        <Suspense
          fallback={
            <div className="h-64 animate-pulse rounded-xl bg-slate-800/50" />
          }
        >
          <LoginForm />
        </Suspense>
      </div>
    </div>
  );
}
