import type {
  WorkCatalogAttentionV1,
  WorkCatalogEntryV1,
  WorkCatalogPageV1,
} from "@astra/sdk";
import { ArrowRight, Inbox, Plus } from "lucide-react";
import Link from "next/link";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils/cn";

const GROUPS: Array<{
  attention: WorkCatalogAttentionV1;
  title: string;
  description: string;
}> = [
  {
    attention: "needs_review",
    title: "Needs you",
    description: "Astra has a concrete decision ready for review.",
  },
  {
    attention: "updated",
    title: "Updated",
    description: "New durable activity since you last opened this Work.",
  },
  {
    attention: "none",
    title: "Current",
    description: "No unread activity or pending decision.",
  },
];

export function WorkNowPage({
  page,
  isLatest,
}: {
  page: WorkCatalogPageV1;
  isLatest: boolean;
}) {
  return (
    <div className="h-full overflow-y-auto">
      <main className="mx-auto w-full max-w-5xl px-5 py-8 sm:px-8 lg:py-12">
        <header className="flex flex-col gap-5 border-b border-border/80 pb-8 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <p className="text-xs font-semibold uppercase tracking-[0.12em] text-text-muted">
              Now
            </p>
            <h1 className="mt-2 text-3xl font-semibold tracking-[-0.035em] text-text">
              Work that can move forward
            </h1>
            <p className="mt-3 max-w-2xl text-sm leading-6 text-text-secondary">
              Decisions first, then unread updates and the rest of your current Work.
            </p>
          </div>
          <Link
            href="/works"
            className="inline-flex h-10 shrink-0 items-center justify-center gap-2 rounded-control bg-text px-4 text-sm font-semibold text-white transition hover:bg-text/90"
          >
            <Plus className="size-4" />
            Start Work
          </Link>
        </header>

        {page.entries.length === 0 ? (
          <Card className="mt-8 flex flex-col items-center px-6 py-14 text-center">
            <div className="flex size-10 items-center justify-center rounded-control bg-surface-muted text-text-muted">
              <Inbox className="size-5" />
            </div>
            <h2 className="mt-4 text-base font-semibold text-text">
              {isLatest ? "No Work yet" : "No older Work"}
            </h2>
            <p className="mt-2 max-w-md text-sm leading-6 text-text-secondary">
              {isLatest
                ? "Start with an outcome. Astra will keep its tasks, decisions, and evidence together."
                : "You have reached the end of this bounded Work history."}
            </p>
          </Card>
        ) : (
          <div className="mt-8 space-y-10">
            {GROUPS.map((group) => {
              const entries = page.entries.filter(
                (entry) => entry.attention === group.attention,
              );
              if (entries.length === 0) return null;
              return (
                <section key={group.attention} aria-labelledby={`now-${group.attention}`}>
                  <div className="flex items-end justify-between gap-4">
                    <div>
                      <h2
                        id={`now-${group.attention}`}
                        className="text-sm font-semibold text-text"
                      >
                        {group.title}
                      </h2>
                      <p className="mt-1 text-xs leading-5 text-text-muted">
                        {group.description}
                      </p>
                    </div>
                    <span className="text-xs tabular-nums text-text-muted">
                      {entries.length}
                    </span>
                  </div>
                  <div className="mt-3 overflow-hidden rounded-card border border-border/80 bg-surface">
                    {entries.map((entry, index) => (
                      <WorkRow
                        key={entry.work_id}
                        entry={entry}
                        separated={index > 0}
                      />
                    ))}
                  </div>
                </section>
              );
            })}
          </div>
        )}

        <footer className="mt-8 flex items-center justify-between gap-3 border-t border-border/70 pt-5">
          {!isLatest ? (
            <Link href="/now" className="text-sm font-medium text-text-secondary hover:text-text">
              Back to latest
            </Link>
          ) : (
            <span />
          )}
          {page.next_cursor ? (
            <Link
              href={olderPageHref(page.next_cursor)}
              className="inline-flex items-center gap-2 text-sm font-medium text-text-secondary hover:text-text"
            >
              Older Work
              <ArrowRight className="size-4" />
            </Link>
          ) : null}
        </footer>
      </main>
    </div>
  );
}

function olderPageHref(cursor: NonNullable<WorkCatalogPageV1["next_cursor"]>) {
  const params = new URLSearchParams({
    before_created_at: cursor.created_at,
    before_work_id: cursor.work_id,
  });
  return `/now?${params.toString()}`;
}

function WorkRow({
  entry,
  separated,
}: {
  entry: WorkCatalogEntryV1;
  separated: boolean;
}) {
  return (
    <Link
      href={`/works/${encodeURIComponent(entry.work_id)}`}
      className={cn(
        "group flex items-center gap-4 px-5 py-4 transition hover:bg-surface-muted/60",
        separated && "border-t border-border/60",
      )}
    >
      <span
        className={cn(
          "size-2 shrink-0 rounded-full",
          entry.attention === "needs_review"
            ? "bg-warning"
            : entry.attention === "updated"
              ? "bg-accent"
              : "bg-border-strong",
        )}
      />
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-text">{entry.goal}</p>
        <p className="mt-1 text-xs tabular-nums text-text-muted">
          {activityLabel(entry.delivery_branch_activity)} · {entry.graph_item_count}{" "}
          {entry.graph_item_count === 1 ? "item" : "items"}
          {entry.pending_decision_count > 0
            ? ` · ${entry.pending_decision_count} to review`
            : entry.unseen_event_count > 0
              ? ` · ${entry.unseen_event_count} new`
              : " · up to date"}
        </p>
      </div>
      <ArrowRight className="size-4 shrink-0 text-text-muted transition group-hover:translate-x-0.5 group-hover:text-text" />
    </Link>
  );
}

function activityLabel(activity: WorkCatalogEntryV1["delivery_branch_activity"]) {
  switch (activity) {
    case "working":
      return "Working";
    case "waiting":
      return "Waiting";
    case "paused":
      return "Paused";
    case "idle":
      return "Idle";
  }
}
