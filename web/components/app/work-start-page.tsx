"use client";

import { ArrowRight, Target } from "lucide-react";
import { useRouter } from "next/navigation";
import { useRef, useState } from "react";
import { startWorkAction } from "@/app/(workspace)/works/actions";
import { Button } from "@/components/ui/button";

type PendingStart = { goal: string; requestId: string };

export function WorkStartPage() {
  const [goal, setGoal] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pending = useRef<PendingStart | null>(null);
  const router = useRouter();

  async function start() {
    const normalizedGoal = goal.trim();
    if (!normalizedGoal || busy) return;
    if (!pending.current || pending.current.goal !== normalizedGoal) {
      pending.current = {
        goal: normalizedGoal,
        requestId: `web-start-work:${crypto.randomUUID()}`,
      };
    }
    setBusy(true);
    setError(null);
    try {
      const result = await startWorkAction(pending.current);
      if (!result.ok) {
        if (result.status === 401) {
          router.push("/login?next=/works");
          return;
        }
        setError(
          result.retryable
            ? "Work was not created. You can safely try again."
            : "This goal could not be used to start Work.",
        );
        return;
      }
      pending.current = null;
      router.push(`/works/${encodeURIComponent(result.workId)}`);
    } catch {
      setError("Work was not created. You can safely try again.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="h-full overflow-y-auto">
      <main className="mx-auto flex min-h-full w-full max-w-4xl flex-col justify-center px-5 py-10 sm:px-8">
        <div className="max-w-2xl">
          <div className="flex size-10 items-center justify-center rounded-control bg-accent/10 text-accent">
            <Target className="size-5" />
          </div>
          <p className="mt-6 text-xs font-semibold uppercase tracking-[0.12em] text-text-muted">
            Start Work
          </p>
          <h1 className="mt-2 text-3xl font-semibold tracking-[-0.035em] text-text sm:text-4xl">
            What outcome should Astra move forward?
          </h1>
          <p className="mt-3 max-w-xl text-sm leading-6 text-text-secondary">
            State the result and important constraints. Astra will begin with one
            durable item, then propose Done-when criteria and tasks as the work
            becomes concrete.
          </p>
        </div>

        <section className="mt-8 max-w-3xl rounded-card border border-border/80 bg-surface p-3 shadow-[0_10px_32px_rgba(15,23,42,0.06)]">
          <textarea
            value={goal}
            onChange={(event) => setGoal(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
                event.preventDefault();
                void start();
              }
            }}
            disabled={busy}
            rows={5}
            className="block min-h-36 w-full resize-y bg-transparent px-3 py-3 text-base leading-7 text-text outline-none placeholder:text-text-muted disabled:opacity-60"
            placeholder="For example: add durable retries to the upload path, cover failure cases, and leave the change ready for review."
            aria-label="Work goal"
          />
          {error ? (
            <p role="alert" className="mx-3 mb-2 rounded-control bg-danger/5 px-3 py-2 text-sm text-danger">
              {error}
            </p>
          ) : null}
          <div className="flex items-center justify-between gap-3 px-2 pb-1 pt-2">
            <p className="text-xs text-text-muted">⌘/Ctrl+Enter to start</p>
            <Button
              variant="primary"
              trailingIcon={ArrowRight}
              disabled={busy || goal.trim().length === 0}
              onClick={() => void start()}
            >
              {busy ? "Starting…" : "Start Work"}
            </Button>
          </div>
        </section>
      </main>
    </div>
  );
}
