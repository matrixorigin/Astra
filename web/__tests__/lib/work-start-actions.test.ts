vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});

import { startWorkAction } from "@/app/(workspace)/works/actions";
import { requireRuntimeClient } from "@/lib/runtime-client";

const requireClient = vi.mocked(requireRuntimeClient);

beforeEach(() => vi.clearAllMocks());

test("starts one canonical Work with no model-authored accepted criteria", async () => {
  const createWork = vi.fn().mockResolvedValue({
    overview: {
      work_id: "work-1",
      delivery_branch: { branch_id: "branch-1" },
    },
  });
  requireClient.mockResolvedValue({ sdk: { createWork } } as never);

  await expect(
    startWorkAction({ requestId: "start-1", goal: "Ship a reliable change" }),
  ).resolves.toEqual({ ok: true, workId: "work-1", branchId: "branch-1" });
  expect(createWork).toHaveBeenCalledWith({
    requestId: "start-1",
    goal: "Ship a reliable change",
    criteria: [],
  });
});

test("returns typed invalid input without contacting a fallback owner", async () => {
  await expect(
    startWorkAction({ requestId: "start-1", goal: "   " }),
  ).resolves.toEqual({
    ok: false,
    status: 400,
    code: "invalid_work_create_request",
    retryable: false,
  });
  expect(requireClient).not.toHaveBeenCalled();
});

test("does not misclassify a malformed runtime contract as user input", async () => {
  const createWork = vi.fn().mockRejectedValue(new TypeError("invalid observation report"));
  requireClient.mockResolvedValue({ sdk: { createWork } } as never);

  await expect(
    startWorkAction({ requestId: "start-1", goal: "Ship a reliable change" }),
  ).rejects.toThrow("invalid observation report");
});
