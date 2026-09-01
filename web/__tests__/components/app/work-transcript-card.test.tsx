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
  expect(screen.getByText(/conversation is catching up/i)).toBeInTheDocument();
  expect(screen.getByText(/large detail omitted/i)).toBeInTheDocument();
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
