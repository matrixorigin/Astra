"use client";

import { useEffect, useState } from "react";

export function ErrorRecovery() {
  const [signIn, setSignIn] = useState("/login");
  useEffect(() => {
    const next = `${window.location.pathname}${window.location.search}${window.location.hash}`;
    setSignIn(`/login?${new URLSearchParams({ next })}`);
  }, []);
  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-4 bg-bg p-6 text-text">
      <h1 className="text-2xl font-semibold">Unable to load this page</h1>
      <p className="max-w-md text-center text-sm text-text-secondary">
        Try again in a moment. If your session needs to be restored, sign in again.
      </p>
      <div className="flex items-center gap-4">
        <a
          href=""
          className="rounded-control bg-text px-5 py-2.5 text-sm font-medium text-white"
        >
          Try again
        </a>
        <a href={signIn} className="text-sm font-medium text-accent underline">
          Sign in
        </a>
      </div>
    </main>
  );
}
