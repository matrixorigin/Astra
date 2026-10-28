"use client";

import { useState } from "react";

export function PreviousSessionLogout({ disabled, onPendingChange }: { disabled: boolean; onPendingChange: (pending: boolean) => void }) {
  const [apiUrl, setApiUrl] = useState("");
  const [status, setStatus] = useState<"idle" | "pending" | "done" | "error">("idle");
  const [action, setAction] = useState<"logout" | "select">("logout");

  async function signOut() {
    setAction("logout");
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

  async function selectRuntime() {
    setAction("select");
    setStatus("pending");
    onPendingChange(true);
    try {
      const response = await fetch("/api/runtime-config", apiUrl.trim() ? {
        method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ apiUrl: apiUrl.trim() }),
      } : { method: "DELETE" });
      setStatus(response.ok ? "done" : "error");
    } catch { setStatus("error"); }
    finally { onPendingChange(false); }
  }

  return (
    <div className="text-sm text-text-secondary">
      <p>Switching accounts? Sign out the previous browser session first to revoke it.</p>
      <button type="button" onClick={signOut} disabled={status === "pending" || disabled} className="mt-2 font-medium text-accent underline">
        {status === "pending" && action === "logout" ? "Signing out…" : "Sign out previous browser session"}
      </button>
      <label className="mt-3 block" htmlFor="runtime-url">Optional runtime URL (leave empty to use current selection)</label>
      <input id="runtime-url" name="runtime_url" value={apiUrl} onChange={(event) => { setApiUrl(event.target.value); setStatus("idle"); }} disabled={disabled || status === "pending"} className="mt-1 w-full rounded-control border border-border bg-surface px-3 py-2" />
      <button type="button" onClick={selectRuntime} disabled={status === "pending" || disabled} className="mt-2 font-medium text-accent underline">{status === "pending" && action === "select" ? "Selecting…" : "Select runtime"}</button>
      <p>To select the deployment default, leave the URL empty and click Select runtime. Changing the selection does not revoke the previous session. Static tokens must be revoked with their issuer.</p>
      {status === "done" && <p role="status">{action === "logout" ? "Previous browser session signed out, if one was selected." : "Runtime selected. Sign in to continue."}</p>}
      {status === "error" && <p role="alert">{action === "logout" ? "Could not sign out the previous session. Please retry." : "Could not select the runtime. Please check the URL and retry."}</p>}
    </div>
  );
}
