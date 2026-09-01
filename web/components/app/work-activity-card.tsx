"use client";

import type { WorkEventKind } from "@astra/sdk";
import { History } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { markWorkSeenAction } from "@/app/(workspace)/works/[workId]/actions";
import { Card } from "@/components/ui/card";
import type { WorkActivitySnapshot } from "@/lib/work-overview";

const EVENT_LABELS: Record<WorkEventKind, string> = {
  work_created: "Work started",
  goal_revised: "Goal updated",
  criteria_accepted: "Done-when criteria accepted",
  branch_basis_adopted: "Branch aligned with current Work",
  graph_replaced: "Plan updated",
  delivery_branch_selected: "Main result changed",
  branch_archived: "Approach archived",
  branch_restored: "Approach restored",
  subject_changed: "Current result changed",
  patch_artifact_exported: "Reviewable patch exported",
  plan_proposed: "Plan expanded",
  criteria_proposed: "Done-when criteria suggested",
  proposal_rejected: "Suggestion rejected",
  check_recorded: "Verification evidence recorded",
  gaps_accepted: "Known gaps accepted for review",
  run_completed: "Astra finished the latest run",
  run_delegated: "Astra delegated the next workstream",
  run_failed: "The latest run stopped with an error",
  run_cancelled: "The latest run was cancelled",
  runtime_events_expired: "Some older runtime updates expired before projection",
};

export function WorkActivityCard({
  workId,
  activity,
}: {
  workId: string;
  activity: WorkActivitySnapshot;
}) {
  const attemptedHead = useRef(0);
  const [syncDeferred, setSyncDeferred] = useState(false);

  useEffect(() => {
    if (
      activity.unseenCount === 0 ||
      attemptedHead.current >= activity.eventHead
    ) {
      return;
    }
    attemptedHead.current = activity.eventHead;
    setSyncDeferred(false);
    void markWorkSeenAction({
      workId,
      throughEventSeq: activity.eventHead,
    })
      .then((result) => setSyncDeferred(!result.ok))
      .catch(() => setSyncDeferred(true));
  }, [activity.eventHead, activity.unseenCount, workId]);

  if (activity.unseenCount === 0) return null;

  return (
    <Card className="overflow-hidden p-0">
      <div className="flex items-start gap-3 border-b border-border/70 px-5 py-4">
        <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
          <History className="size-4" />
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="text-sm font-semibold text-text">Since your last visit</h2>
            <span className="text-xs tabular-nums text-text-muted">
              {activity.unseenCount} new
            </span>
          </div>
          {activity.truncated ? (
            <p className="mt-1 text-xs leading-5 text-text-muted">
              Showing the latest {activity.events.length} updates.
            </p>
          ) : null}
          {syncDeferred ? (
            <p className="mt-1 text-xs leading-5 text-text-muted">
              Seen status will sync when the connection recovers.
            </p>
          ) : null}
        </div>
      </div>
      <ol className="divide-y divide-border/60">
        {[...activity.events].reverse().map((event) => (
          <li key={event.event_seq} className="flex items-center gap-3 px-5 py-3 text-sm">
            <span className="size-1.5 shrink-0 rounded-full bg-accent" />
            <span className="min-w-0 flex-1 text-text-secondary">
              {EVENT_LABELS[event.kind]}
            </span>
          </li>
        ))}
      </ol>
    </Card>
  );
}
