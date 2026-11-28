import type { WorkEventRecordV1 } from "@astra/sdk";
import { render, screen, within } from "@testing-library/react";
import { WorkActivityCard } from "@/components/app/work-activity-card";

const events: WorkEventRecordV1[] = [
  {
    event_seq: 3,
    branch_id: "branch-1",
    kind: "plan_proposed",
    work_revision: 1,
    goal_revision: 1,
    criterion_set_revision: 1,
    branch_revision: 1,
    graph_revision: 1,
    source_ref: "proposal-1",
    created_at: "2026-08-01T00:01:00Z",
  },
  {
    event_seq: 4,
    branch_id: "branch-1",
    kind: "graph_replaced",
    work_revision: null,
    goal_revision: null,
    criterion_set_revision: null,
    branch_revision: null,
    graph_revision: 1,
    source_ref: "proposal-2",
    created_at: "2026-08-01T00:02:00Z",
  },
];

test("renders the bounded retained history newest first", () => {
  render(<WorkActivityCard events={events} />);

  expect(screen.getByRole("heading", { name: "Recent activity" })).toBeVisible();
  expect(screen.getByText("Showing the latest 2 updates.")).toBeVisible();
  expect(within(screen.getByRole("list")).getAllByRole("listitem").map((item) => item.textContent))
    .toEqual(["Plan updated", "Plan expanded"]);
});

test("omits an empty history", () => {
  const { container } = render(<WorkActivityCard events={[]} />);
  expect(container).toBeEmptyDOMElement();
});
