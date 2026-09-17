vi.mock("next/headers", () => ({
  headers: vi.fn().mockResolvedValue(new Headers()),
  cookies: vi.fn().mockResolvedValue({ get: vi.fn().mockReturnValue(undefined) }),
}));

vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});
vi.mock("@/lib/work-overview", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/work-overview")>();
  return { ...original, getWorkBranchPresentation: vi.fn() };
});

import WorkPage from "@/app/(workspace)/works/[workId]/page";
import { AstraApiError } from "@astra/sdk";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { getWorkBranchPresentation } from "@/lib/work-overview";

const requireClient = vi.mocked(requireRuntimeClient);
const loadPresentation = vi.mocked(getWorkBranchPresentation);

beforeEach(() => vi.clearAllMocks());

test("opens a durable read attachment after resolving the public delivery branch", async () => {
  const snapshot = {
    report: {
      overview: { work_id: "work-1", delivery_branch: { branch_id: "branch-1" } },
    },
  } as never;
  const attachment = { schema_version: 1, attachment_id: "attachment-1" } as never;
  const activity = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    branch_revision: 1,
    activity: "idle",
    observed_at: "2026-08-01T00:00:00Z",
  } as never;
  const execution = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    initialized: true,
    generation: 1,
    state: "ready",
    placement: "server",
  } as never;
  const transcript = { schema_version: 1, items: [] } as never;
  const archivedBranches = { schema_version: 1, branches: [] } as never;
  const patchArtifacts = { schema_version: 1, artifacts: [] } as never;
  const recoveryPoints = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    points: [],
    next_cursor: null,
  } as never;
  const selectedBranch = { branch_id: "branch-1", is_delivery: true } as never;
  const catalog = { branches: [selectedBranch] } as never;
  const attachWorkBranch = vi.fn().mockResolvedValue(attachment);
  const getWorkBranchTranscript = vi.fn().mockResolvedValue(transcript);
  const getWorkBranchActivity = vi.fn().mockResolvedValue(activity);
  const getWorkBranchExecution = vi.fn().mockResolvedValue(execution);
  const listArchivedWorkBranches = vi.fn().mockResolvedValue(archivedBranches);
  const listWorkPatchArtifacts = vi.fn().mockResolvedValue(patchArtifacts);
  const listWorkBranchRecoveryPoints = vi.fn().mockResolvedValue(recoveryPoints);
  const patchCommits = { schema_version: 1, operations: [] } as never;
  const listWorkPatchCommits = vi.fn().mockResolvedValue(patchCommits);
  const sdk = {
    attachWorkBranch,
    getWorkBranchActivity,
    getWorkBranchExecution,
    getWorkBranchTranscript,
    listArchivedWorkBranches,
    listWorkPatchArtifacts,
    listWorkBranchRecoveryPoints,
    listWorkPatchCommits,
  } as never;
  requireClient.mockResolvedValue({ sdk } as never);
  loadPresentation.mockResolvedValue({
    snapshot,
    catalog,
    selectedBranch,
  });

  const element = await WorkPage({ params: Promise.resolve({ workId: "work-1" }) });

  expect(loadPresentation).toHaveBeenCalledWith(sdk, "work-1", undefined);
  const attachInput = attachWorkBranch.mock.calls[0]?.[2] as {
    requestId: string;
    clientId: string;
  };
  expect(attachInput.clientId).toMatch(/^[0-9a-f-]{36}$/u);
  expect(attachInput.requestId).toBe(
    `web-open:${attachInput.clientId}:work-1:branch-1`,
  );
  expect(getWorkBranchTranscript).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 50,
  });
  expect(getWorkBranchActivity).toHaveBeenCalledWith("work-1", "branch-1");
  expect(getWorkBranchExecution).toHaveBeenCalledWith("work-1", "branch-1");
  expect(listArchivedWorkBranches).toHaveBeenCalledWith("work-1", { limit: 20 });
  expect(listWorkPatchArtifacts).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 10,
  });
  expect(listWorkPatchCommits).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 10,
  });
  expect(listWorkBranchRecoveryPoints).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 10,
  });
  expect(element.props).toMatchObject({
    initial: snapshot,
    attachment,
    initialActivity: activity,
    initialExecution: execution,
    transcript,
    archivedBranches,
    patchArtifacts,
    patchCommits,
    recoveryPoints,
    branchCatalog: catalog,
    selectedBranch,
  });
});

test("restores durable patch application progress for an alternative branch", async () => {
  const snapshot = { report: { overview: { work_id: "work-1" } } } as never;
  const selectedBranch = { branch_id: "branch-alt", is_delivery: false } as never;
  const deliveryBranch = { branch_id: "branch-main", is_delivery: true } as never;
  const catalog = { branches: [deliveryBranch, selectedBranch] } as never;
  const materializations = { schema_version: 2, operations: [] } as never;
  const listWorkPatchMaterializations = vi.fn().mockResolvedValue(materializations);
  const commits = { schema_version: 1, operations: [] } as never;
  const listWorkPatchCommits = vi.fn().mockResolvedValue(commits);
  const sdk = {
    attachWorkBranch: vi.fn().mockResolvedValue(null),
    getWorkBranchActivity: vi.fn().mockResolvedValue({}),
    getWorkBranchExecution: vi.fn().mockResolvedValue({}),
    getWorkBranchTranscript: vi.fn().mockResolvedValue({}),
    listArchivedWorkBranches: vi.fn().mockResolvedValue({}),
    listWorkPatchArtifacts: vi.fn().mockResolvedValue({}),
    listWorkBranchRecoveryPoints: vi.fn().mockResolvedValue({}),
    listWorkPatchMaterializations,
    listWorkPatchCommits,
  } as never;
  requireClient.mockResolvedValue({ sdk } as never);
  loadPresentation.mockResolvedValue({ snapshot, catalog, selectedBranch });

  const element = await WorkPage({
    params: Promise.resolve({ workId: "work-1" }),
    searchParams: Promise.resolve({ branch: "branch-alt" }),
  });

  expect(listWorkPatchMaterializations).toHaveBeenCalledWith(
    "work-1",
    "branch-main",
    { sourceBranchId: "branch-alt", limit: 10 },
  );
  expect(element.props.patchMaterializations).toBe(materializations);
  expect(listWorkPatchCommits).toHaveBeenCalledWith("work-1", "branch-main", {
    limit: 10,
  });
  expect(element.props.patchCommits).toBe(commits);
});

test("keeps the Work view usable when attachment rejects due to a stale Server", async () => {
  const snapshot = { report: { overview: { work_id: "work-1" } } } as never;
  const selectedBranch = { branch_id: "branch-1", is_delivery: true } as never;
  const catalog = { branches: [selectedBranch] } as never;
  const attachmentError = new AstraApiError(
    400,
    "invalid_work_attachment_request",
    "/v1/works/work-1/branches/branch-1/attachments",
    "invalid_work_attachment_request",
  );
  const sdk = {
    attachWorkBranch: vi.fn().mockRejectedValue(attachmentError),
    getWorkBranchActivity: vi.fn().mockResolvedValue(null),
    getWorkBranchExecution: vi.fn().mockResolvedValue(null),
    getWorkBranchTranscript: vi.fn().mockResolvedValue(null),
    listArchivedWorkBranches: vi.fn().mockResolvedValue(null),
    listWorkPatchArtifacts: vi.fn().mockResolvedValue(null),
    listWorkPatchCommits: vi.fn().mockResolvedValue(null),
    listWorkBranchRecoveryPoints: vi.fn().mockResolvedValue(null),
  } as never;
  requireClient.mockResolvedValue({ sdk } as never);
  loadPresentation.mockResolvedValue({ snapshot, catalog, selectedBranch });

  const element = await WorkPage({ params: Promise.resolve({ workId: "work-1" }) });

  expect(element.props.attachment).toBeNull();
  expect(element.props.attachmentNotice).toMatch(/Server rejected the read attachment request/u);
});

test("keeps a saved Work readable while a read attachment is temporarily unavailable", async () => {
  const snapshot = { report: { overview: { work_id: "work-1" } } } as never;
  const selectedBranch = { branch_id: "branch-1", is_delivery: true } as never;
  const catalog = { branches: [selectedBranch] } as never;
  const attachmentError = new AstraApiError(
    503,
    "work_attach_unavailable",
    "/v1/works/work-1/branches/branch-1/attachments",
    "work_attach_unavailable",
    "availability",
    true,
  );
  const sdk = {
    attachWorkBranch: vi.fn().mockRejectedValue(attachmentError),
    getWorkBranchActivity: vi.fn().mockResolvedValue(null),
    getWorkBranchExecution: vi.fn().mockResolvedValue(null),
    getWorkBranchTranscript: vi.fn().mockResolvedValue(null),
    listArchivedWorkBranches: vi.fn().mockResolvedValue(null),
    listWorkPatchArtifacts: vi.fn().mockResolvedValue(null),
    listWorkPatchCommits: vi.fn().mockResolvedValue(null),
    listWorkBranchRecoveryPoints: vi.fn().mockResolvedValue(null),
  } as never;
  requireClient.mockResolvedValue({ sdk } as never);
  loadPresentation.mockResolvedValue({ snapshot, catalog, selectedBranch });

  const element = await WorkPage({ params: Promise.resolve({ workId: "work-1" }) });

  expect(element.props.attachment).toBeNull();
  expect(element.props.attachmentNotice).toBeUndefined();
});

test("does not turn an attachment authentication error into a partial page", async () => {
  const snapshot = { report: { overview: { work_id: "work-1" } } } as never;
  const selectedBranch = { branch_id: "branch-1", is_delivery: true } as never;
  const catalog = { branches: [selectedBranch] } as never;
  const attachmentError = new AstraApiError(
    401,
    "authentication_required",
    "/v1/works/work-1/branches/branch-1/attachments",
    "authentication_required",
    "authentication",
  );
  const sdk = {
    attachWorkBranch: vi.fn().mockRejectedValue(attachmentError),
    getWorkBranchActivity: vi.fn().mockResolvedValue(null),
    getWorkBranchExecution: vi.fn().mockResolvedValue(null),
    getWorkBranchTranscript: vi.fn().mockResolvedValue(null),
    listArchivedWorkBranches: vi.fn().mockResolvedValue(null),
    listWorkPatchArtifacts: vi.fn().mockResolvedValue(null),
    listWorkPatchCommits: vi.fn().mockResolvedValue(null),
    listWorkBranchRecoveryPoints: vi.fn().mockResolvedValue(null),
  } as never;
  requireClient.mockResolvedValue({ sdk } as never);
  loadPresentation.mockResolvedValue({ snapshot, catalog, selectedBranch });

  await expect(
    WorkPage({ params: Promise.resolve({ workId: "work-1" }) }),
  ).rejects.toBe(attachmentError);
});
