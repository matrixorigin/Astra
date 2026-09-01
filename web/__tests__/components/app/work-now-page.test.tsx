import type { WorkCatalogPageV1 } from "@astra/sdk";
import { render, screen, within } from "@testing-library/react";
import { WorkNowPage } from "@/components/app/work-now-page";

const page: WorkCatalogPageV1 = {
  schema_version: 1,
  entries: [
    {
      work_id: "work-review",
      goal: "Review the proposed completion contract",
      work_revision: 2,
      delivery_branch_id: "branch-review",
      delivery_branch_revision: 2,
      graph_revision: 2,
      graph_item_count: 3,
      pending_decision_count: 1,
      event_head: 4,
      seen_through_event_seq: 3,
      unseen_event_count: 1,
      attention: "needs_review",
      delivery_branch_activity: "waiting",
      created_at: "2026-08-01T02:00:00Z",
      last_activity_at: "2026-08-01T02:01:00Z",
    },
    {
      work_id: "work-updated",
      goal: "Continue the durable implementation",
      work_revision: 1,
      delivery_branch_id: "branch-updated",
      delivery_branch_revision: 1,
      graph_revision: 1,
      graph_item_count: 1,
      pending_decision_count: 0,
      event_head: 2,
      seen_through_event_seq: null,
      unseen_event_count: 2,
      attention: "updated",
      delivery_branch_activity: "working",
      created_at: "2026-08-01T01:00:00Z",
      last_activity_at: "2026-08-01T01:01:00Z",
    },
    {
      work_id: "work-current",
      goal: "Keep the current result available",
      work_revision: 1,
      delivery_branch_id: "branch-current",
      delivery_branch_revision: 1,
      graph_revision: 1,
      graph_item_count: 2,
      pending_decision_count: 0,
      event_head: 3,
      seen_through_event_seq: 3,
      unseen_event_count: 0,
      attention: "none",
      delivery_branch_activity: "idle",
      created_at: "2026-08-01T00:00:00Z",
      last_activity_at: "2026-08-01T00:01:00Z",
    },
  ],
  next_cursor: {
    created_at: "2026-08-01T00:00:00Z",
    work_id: "work-current",
  },
};

test("groups Work by server-owned attention without exposing runtime identity", () => {
  render(<WorkNowPage page={page} isLatest />);

  const needsYou = screen.getByRole("region", { name: "Needs you" });
  expect(within(needsYou).getByText("Review the proposed completion contract")).toBeVisible();
  expect(within(needsYou).getByText(/1 to review/)).toBeVisible();
  expect(screen.getByRole("region", { name: "Updated" })).toHaveTextContent("2 new");
  expect(screen.getByRole("region", { name: "Updated" })).toHaveTextContent("Working");
  expect(needsYou).toHaveTextContent("Waiting");
  expect(screen.getByRole("region", { name: "Current" })).toHaveTextContent("up to date");
  expect(screen.queryByText(/session-/i)).not.toBeInTheDocument();
  expect(screen.getByRole("link", { name: /older work/i })).toHaveAttribute(
    "href",
    "/now?before_created_at=2026-08-01T00%3A00%3A00Z&before_work_id=work-current",
  );
});

test("renders a useful bounded empty state on the latest page", () => {
  render(
    <WorkNowPage
      page={{ schema_version: 1, entries: [], next_cursor: null }}
      isLatest
    />,
  );

  expect(screen.getByText("No Work yet")).toBeVisible();
  expect(screen.getByRole("link", { name: "Start Work" })).toHaveAttribute(
    "href",
    "/works",
  );
  expect(screen.queryByRole("link", { name: /older work/i })).not.toBeInTheDocument();
});
