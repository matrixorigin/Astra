vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  loadWorkTranscriptPageAction: vi.fn(),
}));

import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { WorkTranscriptPageV1 } from "@astra/sdk";
import { loadWorkTranscriptPageAction } from "@/app/(workspace)/works/[workId]/actions";
import { WorkTranscriptCard } from "@/components/app/work-transcript-card";

const loadEarlier = vi.mocked(loadWorkTranscriptPageAction);
const head = {
  completed_turn: 2,
  journal_event_seq: 2,
  conversation_seq: 2,
  canonical_root_hash: "a".repeat(64),
  projection_schema: 2,
  compaction_generation: 0,
  config_version_id: null,
};
const initial: WorkTranscriptPageV1 = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "branch-1",
  sync: "projection_stale",
  canonical_head: { ...head, completed_turn: 3, journal_event_seq: 3, conversation_seq: 3 },
  transcript_cursor: head,
  items: [
    {
      item_seq: 4,
      committed_turn: 2,
      role: "assistant",
      content: "Recent answer",
      content_truncated: true,
      payload: null,
      payload_omitted: false,
      content_hash: "b".repeat(64),
      created_at: "2026-08-01T00:02:00Z",
    },
  ],
  next_before_item_seq: 4,
  has_more: true,
};

beforeEach(() => vi.clearAllMocks());

test("shows the last safe committed prefix without hiding projection lag", () => {
  render(<WorkTranscriptCard workId="work-1" branchId="branch-1" initial={initial} />);

  expect(screen.getByText("Recent answer")).toBeInTheDocument();
  expect(screen.getByText("Astra")).toBeInTheDocument();
  expect(screen.getByText(/Turn 2/)).toBeInTheDocument();
  expect(screen.getByText(/conversation is catching up/i)).toBeInTheDocument();
  expect(screen.getByText(/some detail is omitted/i)).toBeInTheDocument();
});

test("renders committed markdown as readable content instead of a raw paragraph", () => {
  render(
    <WorkTranscriptCard
      workId="work-1"
      branchId="branch-1"
      initial={{
        ...initial,
        sync: "current",
        items: [
          {
            ...initial.items[0]!,
            content: "## Result\n\n- first check\n- second check\n\n`cargo test`",
            content_truncated: false,
          },
        ],
      }}
    />,
  );

  expect(screen.getByRole("heading", { name: "Result" })).toBeInTheDocument();
  expect(screen.getByText("first check")).toBeInTheDocument();
  expect(screen.getByText("cargo test")).toBeInTheDocument();
});

test("prepends the next keyset page and advances its pagination fact", async () => {
  loadEarlier.mockResolvedValue({
    ok: true,
    page: {
      ...initial,
      sync: "current",
      canonical_head: head,
      items: [
        { ...initial.items[0]!, item_seq: 2, committed_turn: 1, content: "Earlier question" },
      ],
      next_before_item_seq: null,
      has_more: false,
    },
  });
  render(<WorkTranscriptCard workId="work-1" branchId="branch-1" initial={initial} />);

  fireEvent.click(screen.getByRole("button", { name: "Earlier" }));
  await waitFor(() => expect(screen.getByText("Earlier question")).toBeInTheDocument());
  expect(loadEarlier).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    beforeItemSeq: 4,
  });
  expect(screen.getAllByText("Recent answer")).toHaveLength(1);
  expect(screen.queryByRole("button", { name: "Earlier" })).not.toBeInTheDocument();
});

test("does not let an earlier-branch response clear the newly selected branch", async () => {
  let finishOld!: (value: {
    ok: false;
    status: number;
    code: string;
    retryable: boolean;
  }) => void;
  loadEarlier.mockReturnValue(
    new Promise((resolve) => {
      finishOld = resolve as typeof finishOld;
    }) as never,
  );
  const { rerender } = render(
    <WorkTranscriptCard workId="work-1" branchId="branch-1" initial={initial} />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Earlier" }));
  const nextBranch = {
    ...initial,
    branch_id: "branch-2",
    sync: "current" as const,
    items: [{ ...initial.items[0]!, item_seq: 8, content: "Branch B answer" }],
    next_before_item_seq: null,
    has_more: false,
  };
  rerender(
    <WorkTranscriptCard
      workId="work-1"
      branchId="branch-2"
      initial={nextBranch}
    />,
  );
  await waitFor(() => expect(screen.getByText("Branch B answer")).toBeInTheDocument());

  finishOld({
    ok: false,
    status: 503,
    code: "transcript_unavailable",
    retryable: true,
  });
  await waitFor(() =>
    expect(screen.queryByText(/earlier conversation could not be loaded/i)).not.toBeInTheDocument(),
  );
  expect(screen.getByText("Branch B answer")).toBeInTheDocument();
});

test("uses a fresh bounded window instead of rendering a disconnected transcript island", async () => {
  const { rerender } = render(
    <WorkTranscriptCard
      workId="work-1"
      branchId="branch-1"
      initial={{
        ...initial,
        sync: "current",
        items: [{ ...initial.items[0]!, item_seq: 40, content: "Older window" }],
      }}
    />,
  );
  rerender(
    <WorkTranscriptCard
      workId="work-1"
      branchId="branch-1"
      initial={{
        ...initial,
        sync: "current",
        canonical_head: {
          ...initial.canonical_head!,
          completed_turn: 5,
          journal_event_seq: 5,
          conversation_seq: 5,
        },
        transcript_cursor: {
          ...initial.transcript_cursor!,
          completed_turn: 5,
          journal_event_seq: 5,
          conversation_seq: 5,
        },
        items: [{ ...initial.items[0]!, item_seq: 80, content: "New window" }],
      }}
    />,
  );

  await waitFor(() => expect(screen.getByText("New window")).toBeInTheDocument());
  expect(screen.queryByText("Older window")).not.toBeInTheDocument();
});
