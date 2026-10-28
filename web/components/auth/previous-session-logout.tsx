"use client";

import { useState } from "react";

export function PreviousSessionLogout({ disabled, onPendingChange }: { disabled: boolean; onPendingChange: (pending: boolean) => void }) {
  const [status, setStatus] = useState<"idle" | "pending" | "done" | "error">("idle");

  async function signOut() {
    setStatus("pending");
    onPendingChange(true);
    try {
      const response = await fetch("/api/runtime-auth/logout", { method: "POST" });
      setStatus(response.ok ? "done" : "error");
    } catch {
      setStatus("error");
    } finally {
      onPendingChange(false);
    }
  }

  return (
    <div className="text-sm text-text-secondary">
      <p>Switching accounts? Sign out the previous browser session first to revoke it.</p>
      <button type="button" onClick={signOut} disabled={status === "pending" || disabled} className="mt-2 font-medium text-accent underline">
        {status === "pending" ? "Signing out…" : "Sign out previous browser session"}
      </button>
      <p>Static tokens must be revoked with their issuer.</p>
      {status === "done" && <p role="status">Previous browser session signed out, if one was selected.</p>}
      {status === "error" && <p role="alert">Could not sign out the previous session. Please retry.</p>}
    </div>
  );
}
