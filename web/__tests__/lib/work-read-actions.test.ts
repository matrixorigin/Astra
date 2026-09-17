vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});

import {
  loadWorkTaskGraphPageAction,
  markWorkSeenAction,
  refreshWorkTaskGraphAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { requireRuntimeClient } from "@/lib/runtime-client";

const requireClient = vi.mocked(requireRuntimeClient);

beforeEach(() => vi.clearAllMocks());

test("advances one exact durable Work read cursor", async () => {
  const receipt = {
    schema_version: 1,
    work_id: "work-1",
    through_event_seq: 7,
    receipt_revision: 2,
    receipt_hash: "sha256:receipt",
    updated_at: "2026-08-01T00:02:00Z",
  } as const;
  const advanceWorkReadCursor = vi.fn().mockResolvedValue(receipt);
  requireClient.mockResolvedValue({ sdk: { advanceWorkReadCursor } } as never);

  await expect(
    markWorkSeenAction({ workId: "work-1", throughEventSeq: 7 }),
  ).resolves.toEqual({ ok: true, receipt });
  expect(advanceWorkReadCursor).toHaveBeenCalledWith("work-1", 7);
});

test("rejects an ambiguous or invalid cursor before acquiring a runtime", async () => {
  await expect(
    markWorkSeenAction({
      workId: "../internal-session",
      throughEventSeq: 7,
    }),
  ).resolves.toMatchObject({
    ok: false,
    status: 400,
    code: "invalid_work_read_cursor_request",
  });
  await expect(
    markWorkSeenAction({ workId: "work-1", throughEventSeq: 0 }),
  ).resolves.toMatchObject({ ok: false, status: 400 });
  expect(requireClient).not.toHaveBeenCalled();
});

test("loads one exact revision-pinned Task Graph continuation", async () => {
  const cursor = { graph_revision: 4, item_offset: 8, dependency_offset: 12 };
  const page = { schema_version: 1, cursor };
  const getWorkTaskGraph = vi.fn().mockResolvedValue(page);
  requireClient.mockResolvedValue({ sdk: { getWorkTaskGraph } } as never);

  await expect(
    loadWorkTaskGraphPageAction({
      workId: "work-1",
      branchId: "branch-1",
      cursor,
    }),
  ).resolves.toEqual({ ok: true, page });
  expect(getWorkTaskGraph).toHaveBeenCalledWith("work-1", "branch-1", {
    cursor,
    itemLimit: 8,
    dependencyLimit: 128,
  });
});

test("refreshes only the bounded current Task Graph head", async () => {
  const page = { schema_version: 1, cursor: { graph_revision: 5 } };
  const getWorkTaskGraph = vi.fn().mockResolvedValue(page);
  requireClient.mockResolvedValue({ sdk: { getWorkTaskGraph } } as never);

  await expect(
    refreshWorkTaskGraphAction({ workId: "work-1", branchId: "branch-1" }),
  ).resolves.toEqual({ ok: true, page });
  expect(getWorkTaskGraph).toHaveBeenCalledWith("work-1", "branch-1", {
    itemLimit: 8,
    dependencyLimit: 128,
  });
});

test("rejects an ambiguous live Task Graph identity before I/O", async () => {
  await expect(
    refreshWorkTaskGraphAction({ workId: "work-1", branchId: "../branch" }),
  ).resolves.toMatchObject({ ok: false, status: 400 });
  expect(requireClient).not.toHaveBeenCalled();
});

test("rejects malformed Task Graph cursors before acquiring a runtime", async () => {
  await expect(
    loadWorkTaskGraphPageAction({
      workId: "work-1",
      branchId: "../branch",
      cursor: { graph_revision: 4, item_offset: 8, dependency_offset: 12 },
    }),
  ).resolves.toMatchObject({
    ok: false,
    status: 400,
    code: "invalid_work_task_graph_query",
  });
  await expect(
    loadWorkTaskGraphPageAction({
      workId: "work-1",
      branchId: "branch-1",
      cursor: { graph_revision: 0, item_offset: 8, dependency_offset: 12 },
    }),
  ).resolves.toMatchObject({ ok: false, status: 400 });
  expect(requireClient).not.toHaveBeenCalled();
});
