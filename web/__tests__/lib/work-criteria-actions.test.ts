import { AstraApiError, type WorkBranchComparisonReportV2 } from "@astra/sdk";

vi.mock("@/lib/runtime-client", async (importOriginal) => {
  const original = await importOriginal<typeof import("@/lib/runtime-client")>();
  return { ...original, requireRuntimeClient: vi.fn() };
});

vi.mock("@/lib/work-overview", () => ({
  getWorkBranchPresentation: vi.fn(),
}));

import {
  abortWorkPatchMaterializationAction,
  abortWorkBranchControlAction,
  abortWorkBranchCreationAction,
  acquireWorkBranchControlAction,
  changeWorkBranchRetentionAction,
  compareWorkBranchesAction,
  commitWorkPatchAction,
  createWorkBranchAction,
  deleteWorkBranchAction,
  forceTakeoverWorkBranchAction,
  exportWorkPatchArtifactAction,
  loadArchivedWorkBranchesAction,
  loadCriteriaProposalAction,
  loadWorkPatchArtifactsAction,
  loadWorkPatchContentAction,
  materializeWorkPatchAction,
  observeWorkBranchControlAction,
  observeWorkBranchCreationAction,
  observeWorkBranchDeletionAction,
  observeWorkPatchMaterializationAction,
  observeWorkPatchCommitAction,
  abortWorkPatchCommitAction,
  resolveCriteriaProposalAction,
  selectWorkDeliveryAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { getWorkBranchPresentation } from "@/lib/work-overview";

const requireClient = vi.mocked(requireRuntimeClient);
const getPresentation = vi.mocked(getWorkBranchPresentation);

const proposal = {
  work_id: "work-1",
  branch_id: "branch-1",
  proposal_id: "proposal-1",
  proposal_seq: 1,
  payload_hash: "sha256:proposal" as const,
  status: "pending" as const,
  basis: {
    work_revision: 2,
    goal_revision: 1,
    criteria_set_revision: 1,
    branch_revision: 1,
    graph_revision: 1,
  },
  member_count: 1,
  source_kind: "model" as const,
  proposed_at: "2026-08-01T00:00:00Z",
  expires_at: "2026-08-08T00:00:00Z",
};

const detail = {
  schema_version: 1 as const,
  proposal,
  members: [
    {
      member_kind: "new" as const,
      criterion_id: "tests-pass",
      definition: {
        kind: "test_check" as const,
        statement: "Relevant tests pass",
        command: "cargo test -p astra-runtime work",
      },
    },
  ],
  resolution: null,
};

beforeEach(() => vi.clearAllMocks());

test("loads patch metadata and content through exact typed identities", async () => {
  const cursor = {
    created_at: "2026-08-02T12:00:00.000001Z",
    patch_artifact_id: "patch-1",
  };
  const page = { schema_version: 1, artifacts: [], next_cursor: null };
  const content = {
    data: "diff --git a/a b/a\n",
    hash: `sha256:${"a".repeat(64)}`,
    bytes: 21,
  };
  const listWorkPatchArtifacts = vi.fn().mockResolvedValue(page);
  const getWorkPatchArtifactContent = vi.fn().mockResolvedValue(content);
  requireClient.mockResolvedValue({
    sdk: { listWorkPatchArtifacts, getWorkPatchArtifactContent },
  } as never);

  await expect(
    loadWorkPatchArtifactsAction({
      workId: "work-1",
      branchId: "branch-1",
      before: cursor,
    }),
  ).resolves.toEqual({ ok: true, page });
  expect(listWorkPatchArtifacts).toHaveBeenCalledWith("work-1", "branch-1", {
    before: cursor,
    limit: 10,
  });
  await expect(
    loadWorkPatchContentAction({
      workId: "work-1",
      branchId: "branch-1",
      patchArtifactId: "patch-1",
    }),
  ).resolves.toEqual({ ok: true, content });
  expect(getWorkPatchArtifactContent).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    "patch-1",
  );
});

test("exports a patch from public branch revisions while Server owns subject authority", async () => {
  const artifact = { schema_version: 1, patch_artifact_id: "patch-1" };
  const exportWorkPatchArtifact = vi.fn().mockResolvedValue(artifact);
  requireClient.mockResolvedValue({ sdk: { exportWorkPatchArtifact } } as never);
  await expect(
    exportWorkPatchArtifactAction({
      workId: "work-1",
      branchId: "branch-1",
      requestId: "export-1",
      expectedBranchRevision: 4,
      expectedGraphRevision: 3,
    }),
  ).resolves.toEqual({ ok: true, artifact });
  expect(exportWorkPatchArtifact).toHaveBeenCalledWith("work-1", "branch-1", {
    requestId: "export-1",
    expectedBranchRevision: 4,
    expectedGraphRevision: 3,
  });
});

test("applies and observes a patch using public target facts only", async () => {
  const operation = {
    schema_version: 2,
    operation_id: "materialization-1",
    target_branch_id: "branch-main",
  };
  const materializeWorkPatch = vi.fn().mockResolvedValue(operation);
  const getWorkPatchMaterialization = vi.fn().mockResolvedValue(operation);
  const abortWorkPatchMaterialization = vi.fn().mockResolvedValue(undefined);
  requireClient.mockResolvedValue({
    sdk: {
      materializeWorkPatch,
      getWorkPatchMaterialization,
      abortWorkPatchMaterialization,
    },
  } as never);
  const admission = {
    workId: "work-1",
    targetBranchId: "branch-main",
    patchArtifactId: "patch-1",
    requestId: "materialize-1",
    expectedTargetBranchRevision: 7,
    expectedTargetGraphRevision: 6,
  };
  await expect(materializeWorkPatchAction(admission)).resolves.toEqual({
    ok: true,
    operation,
  });
  expect(materializeWorkPatch).toHaveBeenCalledWith("work-1", "branch-main", {
    requestId: "materialize-1",
    patchArtifactId: "patch-1",
    expectedTargetBranchRevision: 7,
    expectedTargetGraphRevision: 6,
  });
  const identity = {
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "materialization-1",
  };
  await expect(observeWorkPatchMaterializationAction(identity)).resolves.toEqual({
    ok: true,
    operation,
  });
  await expect(abortWorkPatchMaterializationAction(identity)).resolves.toEqual({ ok: true });
  expect(getWorkPatchMaterialization).toHaveBeenCalledWith(
    "work-1",
    "branch-main",
    "materialization-1",
  );
  expect(abortWorkPatchMaterialization).toHaveBeenCalledWith(
    "work-1",
    "branch-main",
    "materialization-1",
  );

  await expect(
    materializeWorkPatchAction({
      ...admission,
      expectedTargetSubjectRef: "internal/ref",
    } as never),
  ).resolves.toMatchObject({ ok: false, status: 400, retryable: false });
});

test("commits and observes an exact reviewed patch without accepting identity authority", async () => {
  const operation = {
    schema_version: 1,
    operation_id: "commit-1",
    target_branch_id: "branch-main",
  };
  const commitWorkPatch = vi.fn().mockResolvedValue(operation);
  const getWorkPatchCommit = vi.fn().mockResolvedValue(operation);
  const abortWorkPatchCommit = vi.fn().mockResolvedValue(undefined);
  requireClient.mockResolvedValue({
    sdk: { commitWorkPatch, getWorkPatchCommit, abortWorkPatchCommit },
  } as never);
  const admission = {
    workId: "work-1",
    targetBranchId: "branch-main",
    patchArtifactId: "patch-1",
    requestId: "commit-1",
    expectedTargetBranchRevision: 8,
    expectedTargetGraphRevision: 6,
    message: "Commit reviewed changes",
  };
  await expect(commitWorkPatchAction(admission)).resolves.toEqual({
    ok: true,
    operation,
  });
  expect(commitWorkPatch).toHaveBeenCalledWith("work-1", "branch-main", {
    requestId: "commit-1",
    patchArtifactId: "patch-1",
    expectedTargetBranchRevision: 8,
    expectedTargetGraphRevision: 6,
    message: "Commit reviewed changes",
  });
  const identity = {
    workId: "work-1",
    targetBranchId: "branch-main",
    operationId: "commit-1",
  };
  await expect(observeWorkPatchCommitAction(identity)).resolves.toEqual({
    ok: true,
    operation,
  });
  await expect(abortWorkPatchCommitAction(identity)).resolves.toEqual({ ok: true });
  expect(getWorkPatchCommit).toHaveBeenCalledWith("work-1", "branch-main", "commit-1");
  expect(abortWorkPatchCommit).toHaveBeenCalledWith("work-1", "branch-main", "commit-1");
  await expect(
    commitWorkPatchAction({ ...admission, author_email: "forged@example.test" } as never),
  ).resolves.toMatchObject({ ok: false, status: 400, retryable: false });
  await expect(
    commitWorkPatchAction({ ...admission, message: "reviewed\u0085commit" }),
  ).resolves.toMatchObject({ ok: false, status: 400, retryable: false });
  expect(commitWorkPatch).toHaveBeenCalledTimes(1);
});

test("rejects malformed patch cursors before loading runtime state", async () => {
  await expect(
    loadWorkPatchArtifactsAction({
      workId: "work-1",
      branchId: "branch-1",
      before: {
        created_at: "not-a-time",
        patch_artifact_id: "patch-1",
      },
    }),
  ).resolves.toMatchObject({ ok: false, status: 400, retryable: false });
  expect(requireClient).not.toHaveBeenCalled();
});

test("returns typed API failure metadata without exposing backend prose", async () => {
  const getWorkCriteriaProposal = vi.fn().mockRejectedValue(
    new AstraApiError(
      409,
      "database-specific conflict details",
      "/v1/works/work-1/branches/branch-1/criteria-proposals/proposal-1",
      "work_revision_conflict",
      "conflict",
      false,
    ),
  );
  requireClient.mockResolvedValue({
    sdk: { getWorkCriteriaProposal },
  } as never);

  await expect(
    loadCriteriaProposalAction({
      workId: "work-1",
      branchId: "branch-1",
      proposalId: "proposal-1",
    }),
  ).resolves.toEqual({
    ok: false,
    status: 409,
    code: "work_revision_conflict",
    retryable: false,
  });
});

test("dispatches a typed branch retention command without interpreting text", async () => {
  const archiveWorkBranch = vi.fn().mockResolvedValue({
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-2",
    request_id: "archive-1",
    kind: "archive",
    work_revision: 3,
    branch_revision: 2,
    outcome: "applied",
  });
  requireClient.mockResolvedValue({ sdk: { archiveWorkBranch } } as never);

  await expect(
    changeWorkBranchRetentionAction({
      workId: "work-1",
      branchId: "branch-2",
      requestId: "archive-1",
      expectedWorkRevision: 2,
      expectedBranchRevision: 1,
      kind: "archive",
    }),
  ).resolves.toMatchObject({ ok: true, receipt: { kind: "archive" } });
  expect(archiveWorkBranch).toHaveBeenCalledWith("work-1", "branch-2", {
    requestId: "archive-1",
    expectedWorkRevision: 2,
    expectedBranchRevision: 1,
  });
  await expect(
    changeWorkBranchRetentionAction({
      workId: "work-1",
      branchId: "branch-2",
      requestId: "archive-1",
      expectedWorkRevision: 0,
      expectedBranchRevision: 1,
      kind: "archive",
    }),
  ).resolves.toEqual({
    ok: false,
    status: 400,
    code: "invalid_work_branch_action_request",
    retryable: false,
  });
});

test("loads the next bounded archived branch page from its exact cursor", async () => {
  const page = {
    schema_version: 1,
    work_id: "work-1",
    work_revision: 3,
    branches: [],
    next_cursor: null,
  } as const;
  const listArchivedWorkBranches = vi.fn().mockResolvedValue(page);
  requireClient.mockResolvedValue({ sdk: { listArchivedWorkBranches } } as never);
  const before = {
    archived_at: "2026-08-02T00:00:00Z",
    branch_id: "branch-2",
  };

  await expect(
    loadArchivedWorkBranchesAction({ workId: "work-1", before }),
  ).resolves.toEqual({ ok: true, page });
  expect(listArchivedWorkBranches).toHaveBeenCalledWith("work-1", {
    before,
    limit: 20,
  });
});

test("starts and observes branch deletion as one typed durable operation", async () => {
  const pending = {
    schema_version: 1,
    operation_id: "deletion-1",
    work_id: "work-1",
    branch_id: "branch-2",
    state: "pending",
    phase: "session_cleanup",
    outcome: "pending",
    work_revision: 4,
    branch_revision: 3,
    created_at: "2026-08-02T00:00:00Z",
    completed_at: null,
  } as const;
  const deleteWorkBranch = vi.fn().mockResolvedValue(pending);
  const getWorkBranchDeletionOperation = vi.fn().mockResolvedValue(pending);
  requireClient.mockResolvedValue({
    sdk: { deleteWorkBranch, getWorkBranchDeletionOperation },
  } as never);

  await expect(
    deleteWorkBranchAction({
      workId: "work-1",
      branchId: "branch-2",
      requestId: "delete-1",
      expectedWorkRevision: 3,
      expectedBranchRevision: 2,
    }),
  ).resolves.toEqual({ ok: true, operation: pending });
  expect(deleteWorkBranch).toHaveBeenCalledWith("work-1", "branch-2", {
    requestId: "delete-1",
    expectedWorkRevision: 3,
    expectedBranchRevision: 2,
  });

  await expect(
    observeWorkBranchDeletionAction({
      workId: "work-1",
      branchId: "branch-2",
      operationId: "deletion-1",
    }),
  ).resolves.toEqual({ ok: true, operation: pending });
  expect(getWorkBranchDeletionOperation).toHaveBeenCalledWith(
    "work-1",
    "branch-2",
    "deletion-1",
  );
});

test("rejects malformed deletion admission before creating a runtime client", async () => {
  await expect(
    deleteWorkBranchAction({
      workId: "work-1",
      branchId: "branch-2",
      requestId: "delete-1",
      expectedWorkRevision: 0,
      expectedBranchRevision: 2,
    }),
  ).resolves.toEqual({
    ok: false,
    status: 400,
    code: "invalid_work_branch_deletion_request",
    retryable: false,
  });
  expect(requireClient).not.toHaveBeenCalled();
});

test("resolves the observed proposal before refreshing bounded Work projections", async () => {
  const resolveWorkCriteriaProposal = vi.fn().mockResolvedValue({
    ...detail,
    proposal: { ...proposal, status: "rejected" },
    resolution: {
      resolution_ref: "decision-1",
      resolved_at: "2026-08-01T00:01:00Z",
      result_work_revision: null,
      result_criteria_set_revision: null,
    },
  });
  const sdk = { resolveWorkCriteriaProposal };
  getPresentation.mockResolvedValue({
    snapshot: { report: { overview: {} } },
  } as never);
  requireClient.mockResolvedValue({
    sdk,
  } as never);

  const result = await resolveCriteriaProposalAction({
    workId: "work-1",
    branchId: "branch-1",
    proposal,
    decision: "reject",
    requestId: "decision-request-1",
  });

  expect(resolveWorkCriteriaProposal).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    proposal,
    { decision: "reject", requestId: "decision-request-1" },
  );
  expect(getPresentation).toHaveBeenCalledWith(sdk, "work-1", "branch-1");
  expect(resolveWorkCriteriaProposal.mock.invocationCallOrder[0]).toBeLessThan(
    getPresentation.mock.invocationCallOrder[0]!,
  );
  expect(result.ok).toBe(true);
});

test("acquires branch control with exact attachment and causal basis", async () => {
  const operation = {
    schema_version: 2,
    operation_id: "operation-1",
    state: "succeeded",
  };
  const controlWorkBranch = vi.fn().mockResolvedValue(operation);
  requireClient.mockResolvedValue({ sdk: { controlWorkBranch } } as never);

  await expect(
    acquireWorkBranchControlAction({
      workId: "work-1",
      branchId: "branch-1",
      attachmentId: "attachment-1",
      requestId: "control-1",
      expectedBranchRevision: 3,
      expectedControlBasis: {
        writer_epoch: 4,
        canonical_root_hash: "a".repeat(64),
      },
    }),
  ).resolves.toEqual({ ok: true, operation });
  expect(controlWorkBranch).toHaveBeenCalledWith("work-1", "branch-1", {
    requestId: "control-1",
    expectedBranchRevision: 3,
    expectedControlBasis: {
      writer_epoch: 4,
      canonical_root_hash: "a".repeat(64),
    },
    command: {
      kind: "acquire_branch_control",
      attachmentId: "attachment-1",
    },
  });
});

test("reauthenticates before sending one sealed forced takeover", async () => {
  const authorization = {
    proof: "opaque-step-up-proof",
    purpose: "session_forced_takeover",
    expires_in: 300,
  };
  const operation = {
    schema_version: 2,
    operation_id: "operation-force",
    state: "succeeded",
    outcome: "taken_over",
  };
  const reauthenticate = vi.fn().mockResolvedValue(authorization);
  const controlWorkBranch = vi.fn().mockResolvedValue(operation);
  requireClient.mockResolvedValue({ sdk: { reauthenticate, controlWorkBranch } } as never);

  await expect(
    forceTakeoverWorkBranchAction({
      workId: "work-1",
      branchId: "branch-1",
      attachmentId: "attachment-1",
      requestId: "force-1",
      expectedBranchRevision: 3,
      expectedControlBasis: {
        writer_epoch: 4,
        canonical_root_hash: "a".repeat(64),
      },
      password: "correct horse battery staple",
    }),
  ).resolves.toEqual({ ok: true, operation });
  expect(reauthenticate).toHaveBeenCalledWith(
    "correct horse battery staple",
    "session_forced_takeover",
  );
  expect(controlWorkBranch).toHaveBeenCalledWith("work-1", "branch-1", {
    requestId: "force-1",
    expectedBranchRevision: 3,
    expectedControlBasis: {
      writer_epoch: 4,
      canonical_root_hash: "a".repeat(64),
    },
    command: {
      kind: "force_takeover",
      attachmentId: "attachment-1",
      reauthenticationProof: "opaque-step-up-proof",
    },
  });
  expect(reauthenticate.mock.invocationCallOrder[0]).toBeLessThan(
    controlWorkBranch.mock.invocationCallOrder[0]!,
  );
});

test("observes and aborts the same durable control operation", async () => {
  const operation = {
    schema_version: 2,
    operation_id: "operation-force",
    state: "pending",
    progress: { phase: "preparing", abortable: true },
  };
  const getWorkBranchControlOperation = vi.fn().mockResolvedValue(operation);
  const abortWorkBranchControlOperation = vi.fn().mockResolvedValue(undefined);
  requireClient.mockResolvedValue({
    sdk: { getWorkBranchControlOperation, abortWorkBranchControlOperation },
  } as never);
  const input = {
    workId: "work-1",
    branchId: "branch-1",
    operationId: "operation-force",
  };

  await expect(observeWorkBranchControlAction(input)).resolves.toEqual({
    ok: true,
    operation,
  });
  await expect(abortWorkBranchControlAction(input)).resolves.toEqual({ ok: true });
  expect(getWorkBranchControlOperation).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    "operation-force",
  );
  expect(abortWorkBranchControlOperation).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    "operation-force",
  );
});

test("creates an alternative from the exact committed head and stable request identity", async () => {
  const operation = {
    schema_version: 1,
    operation_id: "fork-1",
    state: "succeeded",
    outcome: "created",
  };
  const forkWorkBranch = vi.fn().mockResolvedValue(operation);
  requireClient.mockResolvedValue({ sdk: { forkWorkBranch } } as never);
  const committedCursor = {
    completed_turn: 4,
    journal_event_seq: 9,
    conversation_seq: 12,
    canonical_root_hash: "a".repeat(64),
    projection_schema: 2,
    compaction_generation: 1,
    config_version_id: null,
  };

  await expect(
    createWorkBranchAction({
      workId: "work-1",
      originBranchId: "branch-1",
      requestId: "alternative-1",
      expectedBranchRevision: 3,
      committedCursor,
    }),
  ).resolves.toEqual({ ok: true, operation });
  expect(forkWorkBranch).toHaveBeenCalledWith("work-1", "branch-1", {
    requestId: "alternative-1",
    expectedBranchRevision: 3,
    committedCursor,
  });
});

test("observes and aborts one exact durable alternative operation", async () => {
  const operation = {
    schema_version: 1,
    operation_id: "fork-1",
    state: "pending",
    outcome: "pending",
  };
  const getWorkBranchForkOperation = vi.fn().mockResolvedValue(operation);
  const abortWorkBranchForkOperation = vi.fn().mockResolvedValue(undefined);
  requireClient.mockResolvedValue({
    sdk: { getWorkBranchForkOperation, abortWorkBranchForkOperation },
  } as never);
  const input = {
    workId: "work-1",
    originBranchId: "branch-1",
    operationId: "fork-1",
  };

  await expect(observeWorkBranchCreationAction(input)).resolves.toEqual({
    ok: true,
    operation,
  });
  await expect(abortWorkBranchCreationAction(input)).resolves.toEqual({ ok: true });
  expect(getWorkBranchForkOperation).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    "fork-1",
  );
  expect(abortWorkBranchForkOperation).toHaveBeenCalledWith(
    "work-1",
    "branch-1",
    "fork-1",
  );
});

test("compares two exact branch identities without loading a Work projection", async () => {
  const comparison = {
    schema_version: 1,
    work_id: "work-1",
    left: { branch_id: "branch-2" },
    right: { branch_id: "branch-1" },
  };
  const compareWorkBranches = vi.fn().mockResolvedValue(comparison);
  requireClient.mockResolvedValue({ sdk: { compareWorkBranches } } as never);

  await expect(
    compareWorkBranchesAction({
      workId: "work-1",
      leftBranchId: "branch-2",
      rightBranchId: "branch-1",
    }),
  ).resolves.toEqual({ ok: true, comparison });
  expect(compareWorkBranches).toHaveBeenCalledWith(
    "work-1",
    "branch-2",
    "branch-1",
  );
  expect(getPresentation).not.toHaveBeenCalled();
});

test("rejects contradictory comparison identities before loading runtime state", async () => {
  await expect(
    compareWorkBranchesAction({
      workId: "work-1",
      leftBranchId: "branch-1",
      rightBranchId: "branch-1",
    }),
  ).resolves.toEqual({
    ok: false,
    status: 400,
    code: "invalid_work_branch_comparison_request",
    retryable: false,
  });
  expect(requireClient).not.toHaveBeenCalled();
});

const deliveryComparison: WorkBranchComparisonReportV2 = {
  schema_version: 2,
  work_id: "work-1",
  work_revision: 4,
  directly_comparable: true,
  blockers: [],
  graph_relation: "same" as const,
  subject_relation: "same" as const,
  evidence_relation: "different" as const,
  left: {
    branch_id: "branch-2",
    branch_revision: 3,
    is_delivery: false,
    goal_revision_ref: 2,
    criteria: {
      revision: 2,
      manifest_hash: `sha256:${"a".repeat(64)}` as const,
      member_count: 1,
    },
    graph: {
      basis_revision: 1,
      current_revision: 2,
      manifest_hash: `sha256:${"b".repeat(64)}` as const,
      item_count: 2,
      edge_count: 1,
    },
    subject: {
      subject_ref: "workspace/repository/head",
      subject_revision: `sha256:${"c".repeat(64)}` as const,
      graph_revision: 2,
    },
  },
  right: {
    branch_id: "branch-1",
    branch_revision: 2,
    is_delivery: true,
    goal_revision_ref: 2,
    criteria: {
      revision: 2,
      manifest_hash: `sha256:${"a".repeat(64)}` as const,
      member_count: 1,
    },
    graph: {
      basis_revision: 1,
      current_revision: 2,
      manifest_hash: `sha256:${"b".repeat(64)}` as const,
      item_count: 2,
      edge_count: 1,
    },
    subject: {
      subject_ref: "workspace/repository/head",
      subject_revision: `sha256:${"c".repeat(64)}` as const,
      graph_revision: 2,
    },
  },
  left_evidence: {
    manifest_hash: `sha256:${"d".repeat(64)}` as const,
    required_count: 1,
    fresh_check_count: 1,
    accepted_gap_count: 0,
  },
  right_evidence: {
    manifest_hash: `sha256:${"e".repeat(64)}` as const,
    required_count: 1,
    fresh_check_count: 0,
    accepted_gap_count: 0,
  },
  coverage_gaps: ["change_details", "risks", "time_cost"],
};

test("selects delivery from the complete strictly decoded comparison basis", async () => {
  const receipt = { schema_version: 1, outcome: "selected" };
  const selectWorkDeliveryBranch = vi.fn().mockResolvedValue(receipt);
  requireClient.mockResolvedValue({ sdk: { selectWorkDeliveryBranch } } as never);

  await expect(
    selectWorkDeliveryAction({
      workId: "work-1",
      requestId: "select-result-1",
      comparison: deliveryComparison,
    }),
  ).resolves.toEqual({ ok: true, receipt });
  expect(selectWorkDeliveryBranch).toHaveBeenCalledWith("work-1", {
    requestId: "select-result-1",
    branchId: "branch-2",
    expectedWorkRevision: 4,
    expectedBranchRevision: 3,
    expectedGoalRevision: 2,
    expectedCriteriaSetRevision: 2,
    expectedGraphRevision: 2,
    expectedSubject: {
      graphRevision: 2,
      subjectRef: "workspace/repository/head",
      subjectRevision: `sha256:${"c".repeat(64)}`,
    },
    expectedEvidenceManifestHash: `sha256:${"d".repeat(64)}`,
  });
});

test("rejects untrusted or reversed delivery comparison facts before runtime access", async () => {
  await expect(
    selectWorkDeliveryAction({
      workId: "work-1",
      requestId: "select-result-1",
      comparison: {
        ...deliveryComparison,
        left: { ...deliveryComparison.left, is_delivery: true },
        right: { ...deliveryComparison.right, is_delivery: false },
      },
    }),
  ).resolves.toMatchObject({
    ok: false,
    status: 400,
    code: "invalid_work_action_request",
  });
  expect(requireClient).not.toHaveBeenCalled();
});

test("rejects a structurally invalid committed head before loading runtime state", async () => {
  await expect(
    createWorkBranchAction({
      workId: "work-1",
      originBranchId: "branch-1",
      requestId: "alternative-1",
      expectedBranchRevision: 3,
      committedCursor: {
        completed_turn: 4,
        journal_event_seq: 9,
        conversation_seq: 12,
        canonical_root_hash: "not-a-root",
        projection_schema: 2,
        compaction_generation: 1,
        config_version_id: null,
      },
    }),
  ).resolves.toMatchObject({
    ok: false,
    status: 400,
    code: "invalid_work_branch_creation_request",
  });
  expect(requireClient).not.toHaveBeenCalled();
});

test("rejects malformed control facts before loading a runtime client", async () => {
  await expect(
    acquireWorkBranchControlAction({
      workId: "work-1",
      branchId: "branch-1",
      attachmentId: "attachment-1",
      requestId: "control-1",
      expectedBranchRevision: 3,
      expectedControlBasis: {
        writer_epoch: 4,
        canonical_root_hash: "not-a-root",
      },
    }),
  ).resolves.toEqual({
    ok: false,
    status: 400,
    code: "invalid_work_control_request",
    retryable: false,
  });
  expect(requireClient).not.toHaveBeenCalled();
});
