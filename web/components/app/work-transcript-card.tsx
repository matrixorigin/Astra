"use client";

import type { WorkConversationHeadV1, WorkTranscriptPageV1 } from "@astra/sdk";
import rehypeHighlight from "rehype-highlight";
import rehypeKatex from "rehype-katex";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import remarkMath from "remark-math";
import { useEffect, useRef, useState } from "react";
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

const markdownRemarkPlugins = [remarkGfm, remarkMath];
const markdownRehypePlugins = [rehypeKatex, rehypeHighlight];

function transcriptRoleLabel(role: string): string {
  return roleLabel[role] ?? "Recorded activity";
}

function transcriptRoleClasses(role: string): string {
  switch (role) {
    case "user":
      return "border-accent/25 bg-accent/[0.045]";
    case "assistant":
      return "border-border/80 bg-surface";
    case "tool":
      return "border-border/70 bg-surface-muted/45";
    default:
      return "border-border/70 bg-surface-muted/30";
  }
}

export function WorkTranscriptCard({
  id,
  workId,
  branchId,
  initial,
}: {
  id?: string;
  workId: string;
  branchId: string;
  initial?: WorkTranscriptPageV1 | null;
}) {
  const [page, setPage] = useState(initial ?? null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pageGeneration = useRef(0);

  // `router.refresh()` replaces the server snapshot in the parent without
  // remounting this client component. Reset the local page when that snapshot
  // advances or the user switches branches; otherwise a TUI-created turn is
  // invisible and an in-flight "Earlier" response can be appended to the
  // wrong branch.
  useEffect(() => {
    pageGeneration.current += 1;
    setPage((current) => {
      if (!initial || !current || !sameTranscriptIdentity(current, initial)) {
        return initial ?? null;
      }
      // A server refresh may arrive while the user has opened older turns.
      // Keep that already-read prefix, merge the new committed window, and
      // retain its earlier cursor. A stale response must never move the
      // visible conversation backwards.
      if (compareTranscriptProgress(initial, current) < 0) return current;
      return mergeTranscriptPages(current, initial);
    });
    setLoading(false);
    setError(null);
  }, [initial, workId, branchId]);

  async function loadEarlier() {
    if (!page?.next_before_item_seq || loading) return;
    const generation = pageGeneration.current;
    setLoading(true);
    setError(null);
    try {
      const result = await loadWorkTranscriptPageAction({
        workId,
        branchId,
        beforeItemSeq: page.next_before_item_seq,
      });
      if (generation !== pageGeneration.current) return;
      if (!result.ok) {
        setError(
          result.retryable
            ? "Earlier conversation could not be loaded yet. You can safely retry."
            : "Earlier conversation is not available at this branch revision.",
        );
        return;
      }
      if (
        generation !== pageGeneration.current ||
        result.page.work_id !== workId ||
        result.page.branch_id !== branchId
      ) {
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
      if (generation !== pageGeneration.current) return;
      setError(
        "Earlier conversation could not be loaded yet. You can safely retry.",
      );
    } finally {
      if (generation === pageGeneration.current) setLoading(false);
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
    <Card id={id} className="scroll-mt-6 overflow-hidden p-0">
      <div className="flex items-center justify-between gap-4 px-5 py-4">
        <div>
          <h2 className="text-sm font-semibold text-text">Conversation</h2>
          <p className="mt-1 text-xs text-text-muted">
            Committed messages on this branch
          </p>
        </div>
        {page?.has_more ? (
          <Button
            variant="ghost"
            size="sm"
            disabled={loading}
            onClick={() => void loadEarlier()}
          >
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
        <div
          role="alert"
          className="border-t border-danger/20 bg-danger/5 px-5 py-3 text-sm text-danger"
        >
          {error}
        </div>
      ) : null}

      {!page ? (
        <p className="border-t border-border/70 px-5 py-5 text-sm text-text-muted">
          Conversation history is temporarily unavailable. You can still inspect
          activity and Work facts.
        </p>
      ) : page.items.length === 0 && page.sync !== "corrupt" ? (
        <p className="border-t border-border/70 px-5 py-5 text-sm text-text-muted">
          No committed turns yet.
        </p>
      ) : (
        <div className="divide-y divide-border/60 border-t border-border/70">
          {page.items.map((item) => (
            <article key={item.item_seq} className="px-5 py-4">
              <div
                className={cn(
                  "rounded-card border px-4 py-3.5",
                  transcriptRoleClasses(item.role),
                )}
              >
                <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
                  <p className="text-xs font-semibold text-text">
                    {transcriptRoleLabel(item.role)}
                  </p>
                  <p className="text-[11px] tabular-nums text-text-muted">
                    Turn {item.committed_turn} ·{" "}
                    {formatTranscriptTime(item.created_at)}
                  </p>
                </div>
                <div className="mt-2 min-w-0 text-sm leading-6 text-text-secondary">
                  {item.content ? (
                    <div className="astra-markdown [&_.katex-display]:my-1 [&_pre]:my-2">
                      <ReactMarkdown
                        remarkPlugins={markdownRemarkPlugins}
                        rehypePlugins={markdownRehypePlugins}
                      >
                        {item.content}
                      </ReactMarkdown>
                    </div>
                  ) : (
                    <p className="text-text-muted">
                      Structured activity recorded.
                    </p>
                  )}
                  {item.content_truncated || item.payload_omitted ? (
                    <p className="mt-2 text-xs text-text-muted">
                      Some detail is omitted from this bounded view.
                    </p>
                  ) : null}
                </div>
              </div>
            </article>
          ))}
        </div>
      )}
    </Card>
  );
}

function sameTranscriptIdentity(
  left: WorkTranscriptPageV1,
  right: WorkTranscriptPageV1,
): boolean {
  return left.work_id === right.work_id && left.branch_id === right.branch_id;
}

function compareTranscriptProgress(
  left: WorkTranscriptPageV1,
  right: WorkTranscriptPageV1,
): number {
  const a = left.transcript_cursor ?? left.canonical_head;
  const b = right.transcript_cursor ?? right.canonical_head;
  return compareConversationHeads(a, b);
}

function compareConversationHeads(
  left: WorkConversationHeadV1 | null,
  right: WorkConversationHeadV1 | null,
): number {
  if (left === right) return 0;
  if (left === null) return -1;
  if (right === null) return 1;
  for (const [a, b] of [
    [left.compaction_generation, right.compaction_generation],
    [left.journal_event_seq, right.journal_event_seq],
    [left.conversation_seq, right.conversation_seq],
    [left.completed_turn, right.completed_turn],
  ] as const) {
    if (a !== b) return a < b ? -1 : 1;
  }
  return 0;
}

function mergeTranscriptPages(
  current: WorkTranscriptPageV1,
  incoming: WorkTranscriptPageV1,
): WorkTranscriptPageV1 {
  if (incoming.sync === "corrupt") return incoming;
  if (!transcriptWindowsConnect(current, incoming)) {
    // A bounded refresh can jump forward while the user has already loaded
    // the beginning. Do not render two islands with an invisible gap; the
    // fresh window carries the correct cursor for loading that gap later.
    return incoming;
  }
  const bySeq = new Map(current.items.map((item) => [item.item_seq, item]));
  for (const item of incoming.items) bySeq.set(item.item_seq, item);
  const items = [...bySeq.values()].sort((left, right) => left.item_seq - right.item_seq);
  // Once the user has reached the beginning, never reintroduce a cursor from
  // a newer server window; doing so would request duplicate history.
  const nextBefore = current.has_more ? current.next_before_item_seq : null;
  return {
    ...incoming,
    items,
    next_before_item_seq: nextBefore,
    has_more: nextBefore !== null,
  };
}

function transcriptWindowsConnect(
  current: WorkTranscriptPageV1,
  incoming: WorkTranscriptPageV1,
): boolean {
  if (current.items.length === 0 || incoming.items.length === 0) return true;
  const currentFirst = current.items[0]!.item_seq;
  const currentLast = current.items[current.items.length - 1]!.item_seq;
  const incomingFirst = incoming.items[0]!.item_seq;
  const incomingLast = incoming.items[incoming.items.length - 1]!.item_seq;
  return (
    incomingFirst <= currentLast + 1 && currentFirst <= incomingLast + 1
  );
}

function formatTranscriptTime(value: string): string {
  const timestamp = Date.parse(value);
  if (!Number.isFinite(timestamp)) return "time unavailable";
  return new Intl.DateTimeFormat("en", {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp));
}
