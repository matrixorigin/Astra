import type { WorkEventKind, WorkEventRecordV1 } from "@astra/sdk";
import { History } from "lucide-react";
import { Card } from "@/components/ui/card";

const EVENT_LABELS: Record<WorkEventKind, string> = {
  work_created: "Work started",
  criteria_accepted: "Done-when criteria accepted",
  branch_basis_adopted: "Branch aligned with current Work",
  graph_replaced: "Plan updated",
  delivery_branch_selected: "Main result changed",
  branch_archived: "Approach archived",
  branch_restored: "Approach restored",
  subject_changed: "Current result changed",
  plan_proposed: "Plan expanded",
  criteria_proposed: "Done-when criteria suggested",
  proposal_rejected: "Suggestion rejected",
  check_recorded: "Verification evidence recorded",
  gaps_accepted: "Known gaps accepted for review",
  recovery_point_captured: "Progress saved",
};

export function WorkActivityCard({
  id,
  events,
}: {
  id?: string;
  events: WorkEventRecordV1[];
}) {
  if (events.length === 0) return null;
  const truncated = events[0].event_seq > 1;

  return (
    <Card id={id} className="scroll-mt-6 overflow-hidden p-0">
      <div className="flex items-start gap-3 border-b border-border/70 px-5 py-4">
        <div className="mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent">
          <History className="size-4" />
        </div>
        <div className="min-w-0 flex-1">
          <h2 className="text-sm font-semibold text-text">Recent activity</h2>
          {truncated ? (
            <p className="mt-1 text-xs leading-5 text-text-muted">
              Showing the latest {events.length} updates.
            </p>
          ) : null}
        </div>
      </div>
      <ol className="divide-y divide-border/60">
        {[...events].reverse().map((event) => (
          <li
            key={event.event_seq}
            className="flex items-center gap-3 px-5 py-3 text-sm"
          >
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
