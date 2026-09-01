"use client";

import type { WorkTranscriptPageV1 } from "@astra/sdk";
import { useState } from "react";
import { loadWorkTranscriptPageAction } from "@/app/(workspace)/works/[workId]/actions";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils/cn";

const roleLabel: Record<string, string> = {
  user: "You",
  assistant: "Astra",
  tool: "Tool",
  event: "Activity",
};

export function WorkTranscriptCard({
  workId,
  branchId,
  initial,
}: {
  workId: string;
  branchId: string;
  initial?: WorkTranscriptPageV1 | null;
}) {
  const [page, setPage] = useState(initial ?? null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function loadEarlier() {
    if (!page?.next_before_item_seq || loading) return;
    setLoading(true);
    setError(null);
    try {
      const result = await loadWorkTranscriptPageAction({
        workId,
        branchId,
        beforeItemSeq: page.next_before_item_seq,
      });
      if (!result.ok) {
        setError(
          result.retryable
            ? "Earlier conversation could not be loaded yet. You can safely retry."
            : "Earlier conversation is not available at this branch revision.",
        );
        return;
      }
      setPage((current) => {
        if (!current) return result.page;
        return {
          ...result.page,
          items: [...result.page.items, ...current.items],
        };
      });
    } catch {
      setError("Earlier conversation could not be loaded yet. You can safely retry.");
    } finally {
      setLoading(false);
    }
  }

  const syncMessage =
    page?.sync === "projection_stale"
      ? "Recent committed conversation is catching up. Live progress remains available below."
      : page?.sync === "corrupt"
        ? "Conversation history needs repair before it can be shown safely."
        : page?.sync === "degraded" || page?.sync === "offline"
          ? "Conversation history is temporarily unavailable."
          : null;

  return (
    <Card className="overflow-hidden p-0">
      <div className="flex items-center justify-between gap-4 px-5 py-4">
        <div>
          <h2 className="text-sm font-semibold text-text">Conversation</h2>
          <p className="mt-1 text-xs text-text-muted">Committed messages on this branch</p>
        </div>
        {page?.has_more ? (
          <Button variant="ghost" size="sm" disabled={loading} onClick={() => void loadEarlier()}>
            {loading ? "Loading…" : "Earlier"}
          </Button>
        ) : null}
      </div>

      {syncMessage ? (
        <div
          role={page?.sync === "corrupt" ? "alert" : "status"}
          className={cn(
            "border-t px-5 py-3 text-sm",
            page?.sync === "corrupt"
              ? "border-danger/20 bg-danger/5 text-danger"
              : "border-warning/20 bg-warning/5 text-text-secondary",
          )}
        >
          {syncMessage}
        </div>
      ) : null}

      {error ? (
        <div role="alert" className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger">
          {error}
        </div>
      ) : null}

      {!page ? (
        <p className="border-t border-border/70 px-5 py-5 text-sm text-text-muted">
          Conversation history is temporarily unavailable. You can still inspect activity and Work facts.
        </p>
      ) : page.items.length === 0 && page.sync !== "corrupt" ? (
        <p className="border-t border-border/70 px-5 py-5 text-sm text-text-muted">
          No committed turns yet.
        </p>
      ) : (
        <div className="divide-y divide-border/60 border-t border-border/70">
          {page.items.map((item) => (
            <article key={item.item_seq} className="grid gap-2 px-5 py-4 sm:grid-cols-[72px_minmax(0,1fr)]">
              <p className="text-xs font-semibold text-text-muted">
                {roleLabel[item.role] ?? item.role}
              </p>
              <div className="min-w-0">
                {item.content ? (
                  <p className="whitespace-pre-wrap break-words text-sm leading-6 text-text-secondary">
                    {item.content}
                  </p>
                ) : (
                  <p className="text-sm text-text-muted">Structured activity recorded.</p>
                )}
                {item.content_truncated || item.payload_omitted ? (
                  <p className="mt-2 text-xs text-text-muted">
                    Large detail omitted from this bounded view.
                  </p>
                ) : null}
              </div>
            </article>
          ))}
        </div>
      )}
    </Card>
  );
}
