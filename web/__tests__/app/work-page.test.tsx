vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});
vi.mock("@/lib/work-overview", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/work-overview")>();
  return { ...original, getWorkBranchPresentation: vi.fn() };
});

import WorkPage from "@/app/(workspace)/works/[workId]/page";
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
  const transcript = { schema_version: 1, items: [] } as never;
  const archivedBranches = { schema_version: 1, branches: [] } as never;
  const patchArtifacts = { schema_version: 1, artifacts: [] } as never;
  const selectedBranch = { branch_id: "branch-1", is_delivery: true } as never;
  const catalog = { branches: [selectedBranch] } as never;
  const attachWorkBranch = vi.fn().mockResolvedValue(attachment);
  const getWorkBranchTranscript = vi.fn().mockResolvedValue(transcript);
  const listArchivedWorkBranches = vi.fn().mockResolvedValue(archivedBranches);
  const listWorkPatchArtifacts = vi.fn().mockResolvedValue(patchArtifacts);
  const patchCommits = { schema_version: 1, operations: [] } as never;
  const listWorkPatchCommits = vi.fn().mockResolvedValue(patchCommits);
  const sdk = {
    attachWorkBranch,
    getWorkBranchTranscript,
    listArchivedWorkBranches,
    listWorkPatchArtifacts,
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
  expect(attachWorkBranch).toHaveBeenCalledWith("work-1", "branch-1", {
    requestId: expect.stringMatching(/^web-open:/),
  });
  expect(getWorkBranchTranscript).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 50,
  });
  expect(listArchivedWorkBranches).toHaveBeenCalledWith("work-1", { limit: 20 });
  expect(listWorkPatchArtifacts).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 10,
  });
  expect(listWorkPatchCommits).toHaveBeenCalledWith("work-1", "branch-1", {
    limit: 10,
  });
  expect(element.props).toMatchObject({
    initial: snapshot,
    attachment,
    transcript,
    archivedBranches,
    patchArtifacts,
    patchCommits,
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
    getWorkBranchTranscript: vi.fn().mockResolvedValue({}),
    listArchivedWorkBranches: vi.fn().mockResolvedValue({}),
    listWorkPatchArtifacts: vi.fn().mockResolvedValue({}),
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
