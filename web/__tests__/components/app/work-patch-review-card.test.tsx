import type {
  WorkPatchArtifactPageV1,
  WorkPatchMaterializationOperationV2,
  WorkPatchCommitOperationV1,
} from "@astra/sdk";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  abortWorkPatchMaterializationAction: vi.fn(),
  exportWorkPatchArtifactAction: vi.fn(),
  loadWorkPatchArtifactsAction: vi.fn(),
  loadWorkPatchContentAction: vi.fn(),
  materializeWorkPatchAction: vi.fn(),
  observeWorkPatchMaterializationAction: vi.fn(),
  abortWorkPatchCommitAction: vi.fn(),
  commitWorkPatchAction: vi.fn(),
  observeWorkPatchCommitAction: vi.fn(),
}));

import {
  abortWorkPatchMaterializationAction,
  exportWorkPatchArtifactAction,
  loadWorkPatchArtifactsAction,
  loadWorkPatchContentAction,
  materializeWorkPatchAction,
  observeWorkPatchMaterializationAction,
  abortWorkPatchCommitAction,
  commitWorkPatchAction,
  observeWorkPatchCommitAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { UnifiedDiffView } from "@/components/app/unified-diff-view";
import { WorkPatchReviewCard } from "@/components/app/work-patch-review-card";

const loadContent = vi.mocked(loadWorkPatchContentAction);
const loadArtifacts = vi.mocked(loadWorkPatchArtifactsAction);
const exportArtifact = vi.mocked(exportWorkPatchArtifactAction);
const materializePatch = vi.mocked(materializeWorkPatchAction);
const observeMaterialization = vi.mocked(observeWorkPatchMaterializationAction);
const abortMaterialization = vi.mocked(abortWorkPatchMaterializationAction);
const commitPatch = vi.mocked(commitWorkPatchAction);
const observeCommit = vi.mocked(observeWorkPatchCommitAction);
const abortCommit = vi.mocked(abortWorkPatchCommitAction);
const exportBasis = { branchRevision: 4, graphRevision: 3 };
const materializeTarget = {
  branchId: "branch-main",
  label: "Main result",
  branchRevision: 7,
  graphRevision: 6,
};
const commitTarget = materializeTarget;

const patch = {
  schema_version: 1 as const,
  work_id: "work-1",
  branch_id: "branch-1",
  patch_artifact_id: "patch-1",
  source_branch_revision: 4,
  source_graph_revision: 3,
  base_subject_revision: `sha256:${"a".repeat(64)}` as const,
  result_subject_revision: `sha256:${"b".repeat(64)}` as const,
  payload_hash: `sha256:${"c".repeat(64)}` as const,
  payload_bytes: 90,
  format: "unified_diff_v1" as const,
  provider_invocation_ref: "server-git-export:1",
  source_ref: "export-1",
  created_at: "2026-08-02T12:00:00.000001Z",
};

const page: WorkPatchArtifactPageV1 = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "branch-1",
  artifacts: [patch],
  next_cursor: null,
};

const pendingOperation: WorkPatchMaterializationOperationV2 = {
  schema_version: 2,
  operation_id: "materialization-1",
  work_id: "work-1",
  request_id: "materialize-1",
  patch_artifact_id: patch.patch_artifact_id,
  source_branch_id: patch.branch_id,
  target_branch_id: materializeTarget.branchId,
  target_branch_revision: materializeTarget.branchRevision,
  target_graph_revision: materializeTarget.graphRevision,
  base_subject_revision: patch.base_subject_revision,
  result_subject_revision: patch.result_subject_revision,
  payload_hash: patch.payload_hash,
  provider_ref: "server-git-materialize:1",
  policy_decision_ref: "policy-1",
  state: "pending",
  phase: "awaiting_dispatch",
  apply_invocation_ref: null,
  observed_subject_revision: null,
  apply_outcome: null,
  failure_code: null,
  verification_evidence_hash: null,
  verification_outcome: null,
  created_at: "2026-08-02T12:01:00.000001Z",
  completed_at: null,
};

const succeededOperation: WorkPatchMaterializationOperationV2 = {
  ...pendingOperation,
  state: "succeeded",
  phase: "complete",
  apply_invocation_ref: "apply-1",
  observed_subject_revision: patch.result_subject_revision,
  apply_outcome: "applied",
  verification_evidence_hash: `sha256:${"d".repeat(64)}`,
  verification_outcome: "passed",
  completed_at: "2026-08-02T12:02:00.000001Z",
};

const pendingCommit: WorkPatchCommitOperationV1 = {
  schema_version: 1,
  operation_id: "commit-1",
  work_id: "work-1",
  request_id: "commit-request-1",
  patch_artifact_id: patch.patch_artifact_id,
  source_branch_id: patch.branch_id,
  target_branch_id: commitTarget.branchId,
  target_branch_revision: 8,
  target_graph_revision: commitTarget.graphRevision,
  base_subject_revision: patch.base_subject_revision,
  result_subject_revision: patch.result_subject_revision,
  payload_hash: patch.payload_hash,
  message: "Apply reviewed changes",
  provider_ref: "server-git-worktree-commit-v1",
  policy_decision_ref: "commit-request-1",
  state: "pending",
  phase: "awaiting_dispatch",
  commit_invocation_ref: null,
  commit_sha: null,
  observed_subject_revision: null,
  index_reconciled: null,
  failure_code: null,
  created_at: "2026-08-02T12:03:00.000001Z",
  completed_at: null,
};

beforeEach(() => vi.clearAllMocks());

test("loads exact diff content only when the user opens a review", async () => {
  const data =
    "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-before\n+after\n";
  loadContent.mockResolvedValue({
    ok: true,
    content: { data, hash: patch.payload_hash, bytes: patch.payload_bytes },
  });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
    />,
  );

  expect(screen.queryByRole("table", { name: "Unified diff" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  expect(loadContent).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    patchArtifactId: "patch-1",
  });
  await waitFor(() =>
    expect(screen.getByRole("table", { name: "Unified diff" })).toBeInTheDocument(),
  );
  expect(document.querySelector('[data-diff-kind="deletion"]')).toHaveTextContent("-before");
  expect(document.querySelector('[data-diff-kind="addition"]')).toHaveTextContent("+after");
});

test("fails closed when content metadata disagrees with immutable provenance", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n",
      hash: `sha256:${"d".repeat(64)}`,
      bytes: patch.payload_bytes,
    },
  });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  expect(
    await screen.findByText("Change content does not match its immutable review record."),
  ).toBeInTheDocument();
  expect(screen.queryByRole("table", { name: "Unified diff" })).toBeNull();
});

test("keeps the rendered DOM bounded for a long diff", () => {
  const hunk = "+line\n".repeat(500_000);
  render(<UnifiedDiffView data={`@@ -0,0 +1,500000 @@\n${hunk}`} />);
  const table = screen.getByRole("table", { name: "Unified diff" });
  expect(table).toHaveAttribute("aria-rowcount", "500001");
  expect(table.querySelectorAll('[role="row"]').length).toBeLessThan(100);
});

test("loads earlier metadata through the exact server cursor", async () => {
  const cursor = {
    created_at: patch.created_at,
    patch_artifact_id: patch.patch_artifact_id,
  };
  const older = {
    ...patch,
    patch_artifact_id: "patch-older",
    created_at: "2026-08-02T11:00:00.000001Z",
  };
  loadArtifacts.mockResolvedValue({
    ok: true,
    page: { ...page, artifacts: [older], next_cursor: null },
  });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={{ ...page, next_cursor: cursor }}
      exportBasis={exportBasis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Show earlier exports" }));
  await waitFor(() => expect(screen.getAllByText("Earlier changes")).toHaveLength(1));
  expect(loadArtifacts).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    before: cursor,
  });
});

test("prepares the current exact branch basis without client-owned subject authority", async () => {
  exportArtifact.mockResolvedValue({ ok: true, artifact: patch });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={{ ...page, artifacts: [] }}
      exportBasis={exportBasis}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Prepare review" }));
  await waitFor(() => expect(screen.getByText("Latest changes")).toBeInTheDocument());
  expect(exportArtifact).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    requestId: expect.stringMatching(/^web-patch-export:/u),
    expectedBranchRevision: 4,
    expectedGraphRevision: 3,
  });
  expect(screen.queryByRole("button", { name: "Prepare review" })).toBeNull();
});

test("confirms one exact patch before applying it to the public target basis", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
      hash: patch.payload_hash,
      bytes: patch.payload_bytes,
    },
  });
  materializePatch.mockResolvedValue({ ok: true, operation: succeededOperation });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  await screen.findByRole("button", { name: "Bring to Main result" });
  fireEvent.click(screen.getByRole("button", { name: "Bring to Main result" }));
  expect(materializePatch).not.toHaveBeenCalled();
  expect(screen.getByText(/only if Main result still has the expected base/u)).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Apply changes" }));

  expect(await screen.findByText("Applied and verified")).toBeVisible();
  expect(materializePatch).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    patchArtifactId: "patch-1",
    requestId: expect.stringMatching(/^web-patch-materialization:/u),
    expectedTargetBranchRevision: 7,
    expectedTargetGraphRevision: 6,
  });
});

test("tracks a pending application to its canonical verified outcome", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
      hash: patch.payload_hash,
      bytes: patch.payload_bytes,
    },
  });
  materializePatch.mockResolvedValue({ ok: true, operation: pendingOperation });
  observeMaterialization.mockResolvedValue({
    ok: true,
    operation: succeededOperation,
  });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  fireEvent.click(await screen.findByRole("button", { name: "Bring to Main result" }));
  fireEvent.click(screen.getByRole("button", { name: "Apply changes" }));

  expect(await screen.findByText("Waiting to apply…")).toBeVisible();
  expect(await screen.findByText("Applied and verified", {}, { timeout: 1_500 })).toBeVisible();
  expect(observeMaterialization).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "materialization-1",
  });
});

test("resumes tracking a durable pending application after page refresh", async () => {
  observeMaterialization.mockResolvedValue({
    ok: true,
    operation: succeededOperation,
  });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      initialMaterializations={{
        schema_version: 2,
        work_id: "work-1",
        target_branch_id: "branch-main",
        source_branch_id: "branch-1",
        operations: [pendingOperation],
        next_cursor: null,
      }}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
    />,
  );

  expect(screen.getByText("Applying…")).toBeVisible();
  await waitFor(() => expect(observeMaterialization).toHaveBeenCalledTimes(1), {
    timeout: 1_500,
  });
  expect(observeMaterialization).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "materialization-1",
  });
});

test("keeps tracking after a retryable observation outage", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
      hash: patch.payload_hash,
      bytes: patch.payload_bytes,
    },
  });
  materializePatch.mockResolvedValue({ ok: true, operation: pendingOperation });
  observeMaterialization
    .mockResolvedValueOnce({
      ok: false,
      status: 503,
      code: "work_patch_materialization_degraded",
      retryable: true,
    })
    .mockResolvedValueOnce({ ok: true, operation: succeededOperation });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  fireEvent.click(await screen.findByRole("button", { name: "Bring to Main result" }));
  fireEvent.click(screen.getByRole("button", { name: "Apply changes" }));

  expect(
    await screen.findByText(
      "Application progress is temporarily unavailable; tracking will continue.",
    ),
  ).toBeVisible();
  expect(await screen.findByText("Applied and verified", {}, { timeout: 2_500 })).toBeVisible();
  expect(observeMaterialization).toHaveBeenCalledTimes(2);
});

test("stops through the durable operation and then reads canonical state", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
      hash: patch.payload_hash,
      bytes: patch.payload_bytes,
    },
  });
  const aborted = {
    ...pendingOperation,
    state: "aborted" as const,
    phase: "complete" as const,
    completed_at: "2026-08-02T12:01:01.000001Z",
  };
  materializePatch.mockResolvedValue({ ok: true, operation: pendingOperation });
  abortMaterialization.mockResolvedValue({ ok: true });
  observeMaterialization.mockResolvedValue({ ok: true, operation: aborted });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  fireEvent.click(await screen.findByRole("button", { name: "Bring to Main result" }));
  fireEvent.click(screen.getByRole("button", { name: "Apply changes" }));
  fireEvent.click(await screen.findByRole("button", { name: "Stop" }));

  expect(await screen.findByText("Application stopped")).toBeVisible();
  expect(abortMaterialization).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "materialization-1",
  });
});

test("asks once before committing the exact reviewed result", async () => {
  loadContent.mockResolvedValue({
    ok: true,
    content: {
      data: "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n",
      hash: patch.payload_hash,
      bytes: patch.payload_bytes,
    },
  });
  commitPatch.mockResolvedValue({ ok: true, operation: pendingCommit });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-1"
      initial={page}
      initialMaterializations={{
        schema_version: 2,
        work_id: "work-1",
        target_branch_id: "branch-main",
        source_branch_id: "branch-1",
        operations: [succeededOperation],
        next_cursor: null,
      }}
      initialCommits={{
        schema_version: 1,
        work_id: "work-1",
        target_branch_id: "branch-main",
        operations: [],
        next_cursor: null,
      }}
      exportBasis={exportBasis}
      materializeTarget={materializeTarget}
      commitTarget={commitTarget}
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: /Latest changes/u }));
  fireEvent.click(await screen.findByRole("button", { name: "Commit reviewed changes" }));
  expect(
    screen.getByText(/Create one Git commit from this exact reviewed patch/u),
  ).toBeVisible();
  expect(commitPatch).not.toHaveBeenCalled();
  fireEvent.change(screen.getByRole("textbox", { name: "Commit message" }), {
    target: { value: "Use the reviewed alternative" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Create commit" }));

  await waitFor(() => expect(commitPatch).toHaveBeenCalledTimes(1));
  expect(commitPatch).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    patchArtifactId: "patch-1",
    requestId: expect.stringMatching(/^web-patch-commit:/u),
    expectedTargetBranchRevision: 8,
    expectedTargetGraphRevision: 6,
    message: "Use the reviewed alternative",
  });
  expect(await screen.findByText("Waiting to commit…")).toBeVisible();
});

test("resumes a durable commit after refresh and shows its exact receipt", async () => {
  const succeededCommit: WorkPatchCommitOperationV1 = {
    ...pendingCommit,
    state: "succeeded",
    phase: "complete",
    commit_invocation_ref: "server-git-commit:commit-1",
    commit_sha: "a".repeat(40),
    observed_subject_revision: `sha256:${"e".repeat(64)}`,
    index_reconciled: true,
    completed_at: "2026-08-02T12:04:00.000001Z",
  };
  observeCommit.mockResolvedValue({ ok: true, operation: succeededCommit });
  render(
    <WorkPatchReviewCard
      workId="work-1"
      branchId="branch-main"
      initial={page}
      initialCommits={{
        schema_version: 1,
        work_id: "work-1",
        target_branch_id: "branch-main",
        operations: [pendingCommit],
        next_cursor: null,
      }}
      exportBasis={exportBasis}
      commitTarget={commitTarget}
    />,
  );

  expect(screen.getByText("Committing…")).toBeVisible();
  await waitFor(() => expect(observeCommit).toHaveBeenCalledTimes(1), { timeout: 1_500 });
  expect(observeCommit).toHaveBeenCalledWith({
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "commit-1",
  });
  expect(await screen.findByText("Committed")).toBeVisible();
  expect(abortCommit).not.toHaveBeenCalled();
});
