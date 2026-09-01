vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  markWorkSeenAction: vi.fn(),
}));

import { StrictMode } from "react";
import { render, screen, waitFor } from "@testing-library/react";
import { markWorkSeenAction } from "@/app/(workspace)/works/[workId]/actions";
import { WorkActivityCard } from "@/components/app/work-activity-card";
import type { WorkActivitySnapshot } from "@/lib/work-overview";

const markSeen = vi.mocked(markWorkSeenAction);

const activity: WorkActivitySnapshot = {
  eventHead: 4,
  seenThroughEventSeq: 2,
  retainedFromEventSeq: 1,
  unseenCount: 2,
  truncated: false,
  events: [
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
      kind: "run_failed",
      work_revision: null,
      goal_revision: null,
      criterion_set_revision: null,
      branch_revision: null,
      graph_revision: 1,
      source_ref: "run:run-2",
      created_at: "2026-08-01T00:02:00Z",
    },
  ],
};

beforeEach(() => {
  vi.clearAllMocks();
  markSeen.mockResolvedValue({
    ok: true,
    receipt: {
      schema_version: 1,
      work_id: "work-1",
      through_event_seq: 4,
      receipt_revision: 2,
      receipt_hash: "sha256:receipt",
      updated_at: "2026-08-01T00:03:00Z",
    },
  });
});

test("renders bounded semantic updates and marks the observed head once", async () => {
  render(
    <StrictMode>
      <WorkActivityCard workId="work-1" activity={activity} />
    </StrictMode>,
  );

  expect(screen.getByText("2 new")).toBeInTheDocument();
  expect(screen.getByText("Plan expanded")).toBeInTheDocument();
  expect(screen.getByText("The latest run stopped with an error")).toBeInTheDocument();
  await waitFor(() =>
    expect(markSeen).toHaveBeenCalledWith({
      workId: "work-1",
      throughEventSeq: 4,
    }),
  );
  expect(markSeen).toHaveBeenCalledTimes(1);
});

test("keeps updates visible and defers cursor sync without interrupting the user", async () => {
  markSeen.mockResolvedValue({
    ok: false,
    status: 503,
    code: "work_write_unavailable",
    retryable: true,
  });
  render(<WorkActivityCard workId="work-1" activity={activity} />);

  expect(
    await screen.findByText(/seen status will sync when the connection recovers/i),
  ).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByText("Plan expanded")).toBeInTheDocument();
});

test("does not write a read cursor when there is no unseen activity", () => {
  const { container } = render(
    <WorkActivityCard
      workId="work-1"
      activity={{ ...activity, unseenCount: 0, events: [] }}
    />,
  );

  expect(container).toBeEmptyDOMElement();
  expect(markSeen).not.toHaveBeenCalled();
});
