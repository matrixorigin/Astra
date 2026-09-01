import type {
  WorkCriteriaProposalDetailV1,
  WorkCriteriaProposalSummaryV1,
  WorkBranchAttachmentV1,
  WorkBranchCreationOperationV1,
  WorkBranchComparisonReportV2,
  WorkOverviewV1,
} from "@astra/sdk";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { ComponentProps } from "react";
import type { WorkOverviewSnapshot } from "@/lib/work-overview";

const refresh = vi.fn();
const push = vi.fn();
vi.mock("next/navigation", () => ({
  useRouter: () => ({ refresh, push }),
}));

vi.mock("@/app/(workspace)/works/[workId]/actions", () => ({
  loadCriteriaProposalAction: vi.fn(),
  resolveCriteriaProposalAction: vi.fn(),
  createWorkBranchAction: vi.fn(),
  observeWorkBranchCreationAction: vi.fn(),
  abortWorkBranchCreationAction: vi.fn(),
  compareWorkBranchesAction: vi.fn(),
  selectWorkDeliveryAction: vi.fn(),
  changeWorkBranchRetentionAction: vi.fn(),
  loadArchivedWorkBranchesAction: vi.fn(),
  deleteWorkBranchAction: vi.fn(),
  observeWorkBranchDeletionAction: vi.fn(),
  loadWorkTaskGraphPageAction: vi.fn(),
}));

import {
  abortWorkBranchCreationAction,
  compareWorkBranchesAction,
  changeWorkBranchRetentionAction,
  createWorkBranchAction,
  deleteWorkBranchAction,
  loadCriteriaProposalAction,
  loadArchivedWorkBranchesAction,
  observeWorkBranchCreationAction,
  observeWorkBranchDeletionAction,
  resolveCriteriaProposalAction,
  selectWorkDeliveryAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { WorkOverviewPage } from "@/components/app/work-overview-page";

function TestWorkOverviewPage(
  props: Omit<ComponentProps<typeof WorkOverviewPage>, "branchCatalog" | "selectedBranch">,
) {
  const { branchCatalog, selectedBranch } = mainBranchProps(props.initial);
  return (
    <WorkOverviewPage
      {...props}
      selectedBranch={selectedBranch}
      branchCatalog={branchCatalog}
    />
  );
}

function mainBranchProps(initial: WorkOverviewSnapshot) {
  const workOverview = initial.report.overview;
  const selectedBranch = {
    branch_id: workOverview.delivery_branch.branch_id,
    branch_revision: workOverview.delivery_branch.branch_revision,
    is_delivery: true,
    origin_branch_id: null,
    fork_cursor: null,
    goal_revision_ref: workOverview.goal.revision,
    criteria_set_revision_ref: workOverview.criteria.revision,
    basis_graph_revision: workOverview.delivery_branch.basis_graph_revision,
    current_graph_revision: workOverview.graph.revision,
    materialization: null,
    created_at: workOverview.delivery_branch.created_at,
  } as const;
  return {
    selectedBranch,
    branchCatalog: {
      schema_version: 1 as const,
      work_id: workOverview.work_id,
      work_revision: workOverview.work_revision,
      delivery_branch_id: selectedBranch.branch_id,
      branches: [selectedBranch],
    },
  };
}

function alternativeBranchProps(initial: WorkOverviewSnapshot) {
  const { selectedBranch: deliveryBranch, branchCatalog } = mainBranchProps(initial);
  const selectedBranch = {
    ...deliveryBranch,
    branch_id: "branch-2",
    is_delivery: false,
    origin_branch_id: deliveryBranch.branch_id,
    fork_cursor: `sha256:${"b".repeat(64)}` as const,
    materialization: [
      { dimension: "conversation" as const, disposition: "shared" as const },
      { dimension: "goal" as const, disposition: "shared" as const },
      { dimension: "criteria" as const, disposition: "shared" as const },
      { dimension: "task_graph" as const, disposition: "shared" as const },
      { dimension: "checkpoint" as const, disposition: "gap" as const },
      { dimension: "workspace" as const, disposition: "gap" as const },
      { dimension: "artifacts" as const, disposition: "gap" as const },
      {
        dimension: "transient_authority" as const,
        disposition: "excluded" as const,
      },
    ],
    created_at: "2026-08-01T00:01:00Z",
  };
  initial.taskGraph.basis.branch_id = selectedBranch.branch_id;
  initial.proposals.branch_id = selectedBranch.branch_id;
  return {
    selectedBranch,
    branchCatalog: {
      ...branchCatalog,
      branches: [deliveryBranch, selectedBranch],
    },
  };
}

const loadProposal = vi.mocked(loadCriteriaProposalAction);
const resolveProposal = vi.mocked(resolveCriteriaProposalAction);
const createBranch = vi.mocked(createWorkBranchAction);
const observeBranch = vi.mocked(observeWorkBranchCreationAction);
const abortBranch = vi.mocked(abortWorkBranchCreationAction);
const compareBranches = vi.mocked(compareWorkBranchesAction);
const selectDelivery = vi.mocked(selectWorkDeliveryAction);
const changeRetention = vi.mocked(changeWorkBranchRetentionAction);
const loadArchivedBranches = vi.mocked(loadArchivedWorkBranchesAction);
const deleteBranch = vi.mocked(deleteWorkBranchAction);
const observeDeletion = vi.mocked(observeWorkBranchDeletionAction);

const proposal: WorkCriteriaProposalSummaryV1 = {
  work_id: "work-1",
  branch_id: "branch-1",
  proposal_id: "proposal-1",
  proposal_seq: 1,
  payload_hash: "sha256:proposal",
  status: "pending",
  basis: {
    work_revision: 2,
    goal_revision: 1,
    criteria_set_revision: 1,
    branch_revision: 1,
    graph_revision: 1,
  },
  member_count: 2,
  source_kind: "model",
  proposed_at: "2026-08-01T00:00:00Z",
  expires_at: "2026-08-08T00:00:00Z",
};

const detail: WorkCriteriaProposalDetailV1 = {
  schema_version: 1,
  proposal,
  members: [
    {
      member_kind: "new",
      criterion_id: "tests-pass",
      definition: {
        kind: "test_check",
        statement: "Relevant tests pass",
        command: "cargo test -p astra-runtime work",
      },
    },
    {
      member_kind: "new",
      criterion_id: "review-complete",
      definition: {
        kind: "human_review",
        statement: "The change is ready for human review",
      },
    },
  ],
  resolution: null,
};

function overview(criteriaCount = 0): WorkOverviewV1 {
  return {
    work_id: "work-1",
    work_revision: 2,
    project_id: null,
    original_intent_ref: "intent-1",
    goal: { revision: 1, goal: "Ship a reliable change" },
    criteria: {
      revision: criteriaCount > 0 ? 2 : 1,
      member_count: criteriaCount,
      manifest_hash: "sha256:criteria",
    },
    delivery_branch: {
      work_id: "work-1",
      branch_id: "branch-1",
      branch_revision: 1,
      origin_branch_id: null,
      fork_cursor: null,
      goal_revision_ref: 1,
      goal_alignment: "current",
      criteria_set_revision_ref: criteriaCount > 0 ? 2 : 1,
      criteria_alignment: "current",
      basis_graph_revision: 1,
      current_graph_revision: 1,
      retention_state: "active",
      created_at: "2026-08-01T00:00:00Z",
      archived_at: null,
    },
    graph: {
      revision: 1,
      item_count: 1,
      edge_count: 0,
      manifest_hash: "sha256:graph",
    },
    delivery: {
      status: criteriaCount > 0 ? "verification_required" : "criteria_not_accepted",
      required_criterion_count: criteriaCount,
      satisfied_criterion_count: 0,
      fresh_check_count: 0,
      accepted_gap_count: 0,
      remaining_criterion_count: criteriaCount,
      subject_revision: criteriaCount > 0 ? "sha256:subject" : null,
      freshness_valid_until: null,
    },
    event_head: 3,
    retention_state: "active",
    created_at: "2026-08-01T00:00:00Z",
    archived_at: null,
  };
}

function snapshot({
  proposalInbox = [proposal],
  criteriaCount = 0,
}: {
  proposalInbox?: WorkCriteriaProposalSummaryV1[];
  criteriaCount?: number;
} = {}): WorkOverviewSnapshot {
  const workOverview = overview(criteriaCount);
  return {
    report: {
      schema_version: 1,
      report_id: "work-observation:report-1",
      content_hash: "sha256:report",
      scope: "declared_work",
      as_of: {
        work_revision: workOverview.work_revision,
        goal_revision: workOverview.goal.revision,
        criteria_set_revision: workOverview.criteria.revision,
        delivery_branch_revision: workOverview.delivery_branch.branch_revision,
        graph_revision: workOverview.graph.revision,
        event_head: workOverview.event_head,
      },
      source_revisions: [],
      coherence: "coherent",
      coverage_gaps: [],
      finding:
        criteriaCount > 0
          ? {
              fact_code: "verification_required",
              cause_code: "current_evidence_incomplete",
            }
          : {
              fact_code: "criteria_not_accepted",
              cause_code: "accepted_criteria_empty",
            },
      satisfaction_evidence_refs: [],
      overview: workOverview,
    },
    criteria: {
      schema_version: 1,
      basis: {
        work_id: "work-1",
        work_revision: workOverview.work_revision,
        criteria_set_revision: workOverview.criteria.revision,
        manifest_hash: workOverview.criteria.manifest_hash,
        member_count: criteriaCount,
      },
      cursor: {
        criteria_set_revision: workOverview.criteria.revision,
        offset: 0,
      },
      next_cursor: null,
      criteria: {
        offset: 0,
        limit: 8,
        total: criteriaCount,
        entries:
          criteriaCount > 0
            ? [
                {
                  criterion_id: "tests-pass",
                  revision: 1,
                  definition_hash: "sha256:test-definition",
                  kind: "test_check",
                  statement: "Relevant tests pass",
                  command: "cargo test -p astra-runtime work",
                },
              ]
            : [],
      },
    },
    proposals: {
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      proposals: proposalInbox,
    },
    taskGraph: {
      schema_version: 2,
      scope: "declared_work",
      basis: {
        work_id: "work-1",
        work_revision: workOverview.work_revision,
        goal_revision: workOverview.goal.revision,
        goal: workOverview.goal.goal,
        criteria_set_revision: workOverview.criteria.revision,
        criteria_member_count: criteriaCount,
        criteria_manifest_hash: workOverview.criteria.manifest_hash,
        branch_id: "branch-1",
        branch_revision: workOverview.delivery_branch.branch_revision,
        branch_goal_revision: workOverview.goal.revision,
        branch_criteria_set_revision: workOverview.criteria.revision,
        branch_basis_graph_revision: workOverview.graph.revision,
        graph_revision: workOverview.graph.revision,
        graph_item_count: 1,
        graph_edge_count: 0,
        graph_manifest_hash: workOverview.graph.manifest_hash,
      },
      cursor: {
        graph_revision: workOverview.graph.revision,
        item_offset: 0,
        dependency_offset: 0,
      },
      next_cursor: null,
      items: {
        offset: 0,
        limit: 8,
        total: 1,
        entries: [
          {
            item_id: "root",
            revision: 1,
            kind: "milestone",
            objective: "Implement the change",
            expected_result: "A reviewable implementation",
            declaration_state: "active",
            execution: { status: "not_started", terminal: false, run: null },
            delivery: {
              status: "unreported",
              summary: null,
              blocker_kind: null,
              unavailable_capabilities: [],
            },
            verification: { status: "unknown", latest_check: null },
          },
        ],
      },
      dependencies: { offset: 0, limit: 16, total: 0, entries: [] },
    },
    activity: {
      eventHead: workOverview.event_head,
      seenThroughEventSeq: workOverview.event_head,
      retainedFromEventSeq: 1,
      unseenCount: 0,
      truncated: false,
      events: [],
    },
  };
}

function committedAttachment(): WorkBranchAttachmentV1 {
  return {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    attachment_id: "attachment-1",
    attachment_epoch: 1,
    branch_revision: 1,
    mode: "read_only",
    sync: "current",
    control_basis: {
      writer_epoch: 1,
      canonical_root_hash: "a".repeat(64),
    },
    head: {
      completed_turn: 2,
      journal_event_seq: 4,
      conversation_seq: 4,
      canonical_root_hash: "a".repeat(64),
      projection_schema: 2,
      compaction_generation: 0,
      config_version_id: null,
    },
    attached_at: "2026-08-01T00:00:00Z",
    expires_at: "2026-08-01T00:15:00Z",
  };
}

function forkOperation(
  overrides: Partial<WorkBranchCreationOperationV1> = {},
): WorkBranchCreationOperationV1 {
  return {
    schema_version: 1 as const,
    operation_id: "fork-1",
    work_id: "work-1",
    origin_branch_id: "branch-1",
    child_branch_id: "branch-2",
    fork_cursor: `sha256:${"a".repeat(64)}` as const,
    state: "succeeded" as const,
    outcome: "created" as const,
    origin_branch_revision: 1,
    created_at: "2026-08-01T00:00:00Z",
    completed_at: "2026-08-01T00:00:01Z",
    ...overrides,
  };
}

function comparison(
  overrides: Partial<WorkBranchComparisonReportV2> = {},
): WorkBranchComparisonReportV2 {
  const basis = {
    goal_revision_ref: 1,
    criteria: {
      revision: 1,
      manifest_hash: `sha256:${"c".repeat(64)}` as const,
      member_count: 2,
    },
    graph: {
      basis_revision: 1,
      current_revision: 1,
      manifest_hash: `sha256:${"d".repeat(64)}` as const,
      item_count: 3,
      edge_count: 2,
    },
    subject: null,
  };
  return {
    schema_version: 2,
    work_id: "work-1",
    work_revision: 2,
    directly_comparable: true,
    blockers: [],
    graph_relation: "same",
    subject_relation: "unavailable",
    evidence_relation: "same",
    left: {
      branch_id: "branch-2",
      branch_revision: 1,
      is_delivery: false,
      ...basis,
    },
    right: {
      branch_id: "branch-1",
      branch_revision: 1,
      is_delivery: true,
      ...basis,
    },
    left_evidence: {
      manifest_hash: `sha256:${"e".repeat(64)}`,
      required_count: 2,
      fresh_check_count: 0,
      accepted_gap_count: 0,
    },
    right_evidence: {
      manifest_hash: `sha256:${"e".repeat(64)}`,
      required_count: 2,
      fresh_check_count: 0,
      accepted_gap_count: 0,
    },
    coverage_gaps: ["change_details", "risks", "time_cost"],
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  Object.defineProperty(globalThis.crypto, "randomUUID", {
    configurable: true,
    value: vi.fn(() => "00000000-0000-4000-8000-000000000001"),
  });
});

test("keeps proposal payload lazy until the user opens the suggestion", async () => {
  loadProposal.mockResolvedValue({ ok: true, detail });
  render(<TestWorkOverviewPage initial={snapshot()} />);

  expect(loadProposal).not.toHaveBeenCalled();
  expect(screen.getByText(/work can continue while you review/i)).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: /review 2 completion criteria/i }));

  expect(await screen.findByText("Relevant tests pass")).toBeInTheDocument();
  expect(screen.getByText("cargo test -p astra-runtime work")).toBeInTheDocument();
  expect(loadProposal).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    proposalId: "proposal-1",
  });
});

test("does not present execution completion as verified work", () => {
  const value = snapshot({ proposalInbox: [] });
  value.taskGraph.items.entries[0] = {
    ...value.taskGraph.items.entries[0]!,
    execution: {
      status: "completed",
      terminal: true,
      run: {
        run_id: "run-1",
        attempt_id: "run-1",
        graph_revision: 1,
        run_generation: 1,
        last_event_idx: 4,
        updated_at: "2026-08-01T00:01:00Z",
      },
    },
    verification: { status: "unknown", latest_check: null },
  };

  render(<TestWorkOverviewPage initial={value} />);

  expect(screen.getByText("Result not reported")).toBeInTheDocument();
  expect(screen.queryByText("Verified")).not.toBeInTheDocument();
});

test("retries a failed lazy detail load without closing the review", async () => {
  loadProposal
    .mockResolvedValueOnce({
      ok: false,
      status: 503,
      code: "temporarily_unavailable",
      retryable: true,
    })
    .mockResolvedValueOnce({ ok: true, detail });
  render(<TestWorkOverviewPage initial={snapshot()} />);

  fireEvent.click(screen.getByRole("button", { name: /review 2 completion criteria/i }));
  expect(await screen.findByRole("button", { name: "Try loading again" })).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Try loading again" }));
  expect(await screen.findByText("Relevant tests pass")).toBeInTheDocument();
  expect(loadProposal).toHaveBeenCalledTimes(2);
});

test("accepts the exact observed proposal and replaces the bounded snapshot", async () => {
  loadProposal.mockResolvedValue({ ok: true, detail });
  const acceptedSnapshot = snapshot({ proposalInbox: [], criteriaCount: 1 });
  resolveProposal.mockResolvedValue({
    ok: true,
    detail: {
      ...detail,
      proposal: { ...proposal, status: "accepted" },
      resolution: {
        resolution_ref: "decision-1",
        resolved_at: "2026-08-01T00:01:00Z",
        result_work_revision: 3,
        result_criteria_set_revision: 2,
      },
    },
    snapshot: acceptedSnapshot,
  });
  render(<TestWorkOverviewPage initial={snapshot()} />);

  fireEvent.click(screen.getByRole("button", { name: /review 2 completion criteria/i }));
  fireEvent.click(await screen.findByRole("button", { name: "Accept criteria" }));

  await waitFor(() => expect(resolveProposal).toHaveBeenCalledTimes(1));
  expect(resolveProposal).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-1",
    proposal,
    decision: "accept",
    requestId: "work-criteria:accept:00000000-0000-4000-8000-000000000001",
  });
  expect((await screen.findAllByText("Checks needed")).length).toBeGreaterThan(0);
  expect(screen.queryByText("Suggested Done when")).not.toBeInTheDocument();
  expect(screen.getByText("Relevant tests pass")).toBeInTheDocument();
});

test("shows committed continuity without inferring activity from delivery state", () => {
  render(
    <TestWorkOverviewPage
      initial={snapshot()}
      attachment={
        {
          schema_version: 1,
          work_id: "work-1",
          branch_id: "branch-1",
          attachment_id: "attachment-1",
          attachment_epoch: 1,
          branch_revision: 1,
          mode: "read_only",
          sync: "current",
          control_basis: {
            writer_epoch: 1,
            canonical_root_hash: "a".repeat(64),
          },
          head: {
            completed_turn: 2,
            journal_event_seq: 4,
            conversation_seq: 4,
            canonical_root_hash: "a".repeat(64),
            projection_schema: 2,
            compaction_generation: 0,
            config_version_id: null,
          },
          attached_at: "2026-08-01T00:00:00Z",
          expires_at: "2026-08-01T00:15:00Z",
        }
      }
    />,
  );

  expect(screen.getByText("Synced · 2 committed turns")).toBeVisible();
  expect(screen.getAllByText("Criteria open").length).toBeGreaterThan(0);
  expect(screen.queryByText(/^Working$/)).not.toBeInTheDocument();
});

test("reuses the action identity after a retryable failure", async () => {
  loadProposal.mockResolvedValue({ ok: true, detail });
  resolveProposal
    .mockResolvedValueOnce({
      ok: false,
      status: 503,
      code: "temporarily_unavailable",
      retryable: true,
    })
    .mockResolvedValueOnce({
      ok: true,
      detail: {
        ...detail,
        proposal: { ...proposal, status: "rejected" },
        resolution: {
          resolution_ref: "decision-2",
          resolved_at: "2026-08-01T00:01:00Z",
          result_work_revision: null,
          result_criteria_set_revision: null,
        },
      },
      snapshot: snapshot({ proposalInbox: [] }),
    });
  render(<TestWorkOverviewPage initial={snapshot()} />);

  fireEvent.click(screen.getByRole("button", { name: /review 2 completion criteria/i }));
  fireEvent.click(await screen.findByRole("button", { name: "Reject suggestion" }));
  expect(await screen.findByText(/safely try it again/i)).toBeInTheDocument();

  fireEvent.click(screen.getByRole("button", { name: "Reject suggestion" }));
  await waitFor(() => expect(resolveProposal).toHaveBeenCalledTimes(2));
  expect(resolveProposal.mock.calls[0]?.[0].requestId).toBe(
    resolveProposal.mock.calls[1]?.[0].requestId,
  );
});

test("refreshes typed stale decisions without interpreting error text", async () => {
  loadProposal.mockResolvedValue({ ok: true, detail });
  resolveProposal.mockResolvedValue({
    ok: false,
    status: 409,
    code: "work_revision_conflict",
    retryable: false,
  });
  render(<TestWorkOverviewPage initial={snapshot()} />);

  fireEvent.click(screen.getByRole("button", { name: /review 2 completion criteria/i }));
  fireEvent.click(await screen.findByRole("button", { name: "Accept criteria" }));

  expect(await screen.findByText(/work changed before this decision/i)).toBeInTheDocument();
  expect(refresh).toHaveBeenCalledTimes(1);
});

test("creates an alternative only from the exact durable attachment head", async () => {
  const attachment = committedAttachment();
  createBranch.mockResolvedValue({ ok: true, operation: forkOperation() });
  render(<TestWorkOverviewPage initial={snapshot()} attachment={attachment} />);

  fireEvent.click(screen.getByRole("button", { name: "Try another approach" }));

  await waitFor(() => expect(createBranch).toHaveBeenCalledTimes(1));
  expect(createBranch).toHaveBeenCalledWith({
    workId: "work-1",
    originBranchId: "branch-1",
    requestId: "work-alternative:00000000-0000-4000-8000-000000000001",
    expectedBranchRevision: 1,
    committedCursor: attachment.head,
  });
  expect(push).toHaveBeenCalledWith("/works/work-1?branch=branch-2");
});

test("does not invent a fork boundary before any turn is committed", () => {
  render(<TestWorkOverviewPage initial={snapshot()} />);

  expect(screen.getByRole("button", { name: "Try another approach" })).toBeDisabled();
  expect(createBranch).not.toHaveBeenCalled();
});

test("switches only to an exact active branch identity", () => {
  const value = snapshot();
  const { selectedBranch, branchCatalog } = alternativeBranchProps(value);
  render(
    <WorkOverviewPage
      initial={value}
      selectedBranch={selectedBranch}
      branchCatalog={branchCatalog}
    />,
  );

  fireEvent.change(screen.getByLabelText("Work approach"), {
    target: { value: "branch-1" },
  });

  expect(push).toHaveBeenCalledWith("/works/work-1?branch=branch-1");
  expect(screen.getByRole("option", { name: "Alternative 1" })).toBeInTheDocument();
  expect(screen.getByText("conversation, goal, Done when, plan")).toBeVisible();
  expect(screen.getByText("checkpoint, workspace, artifacts")).toBeVisible();
  expect(screen.getByText("active runs and approvals")).toBeVisible();
});

test("archives a non-main approach with its exact aggregate and branch revisions", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  changeRetention.mockResolvedValue({
    ok: true,
    receipt: {
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-2",
      request_id: "work-branch:archive:00000000-0000-4000-8000-000000000001",
      kind: "archive",
      work_revision: 3,
      branch_revision: 2,
      outcome: "applied",
    },
  });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(screen.getByRole("button", { name: "Archive" }));

  await waitFor(() => expect(changeRetention).toHaveBeenCalledTimes(1));
  expect(changeRetention).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-2",
    requestId: "work-branch:archive:00000000-0000-4000-8000-000000000001",
    expectedWorkRevision: 2,
    expectedBranchRevision: 1,
    kind: "archive",
  });
  expect(push).toHaveBeenCalledWith("/works/work-1?branch=branch-1");
});

test("restores an archived approach from the bounded archive surface", async () => {
  const value = snapshot();
  const branchProps = mainBranchProps(value);
  changeRetention.mockResolvedValue({
    ok: true,
    receipt: {
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-2",
      request_id: "work-branch:restore:00000000-0000-4000-8000-000000000001",
      kind: "restore",
      work_revision: 4,
      branch_revision: 3,
      outcome: "applied",
    },
  });
  loadArchivedBranches.mockResolvedValue({
    ok: true,
    page: {
      schema_version: 1,
      work_id: "work-1",
      work_revision: 3,
      branches: [
        {
          branch_id: "branch-3",
          branch_revision: 2,
          origin_branch_id: "branch-1",
          archived_at: "2026-08-01T00:00:00Z",
          created_at: "2026-07-31T00:00:00Z",
        },
      ],
      next_cursor: null,
    },
  });
  render(
    <WorkOverviewPage
      initial={value}
      {...branchProps}
      archivedBranches={{
        schema_version: 1,
        work_id: "work-1",
        work_revision: 3,
        branches: [
          {
            branch_id: "branch-2",
            branch_revision: 2,
            origin_branch_id: "branch-1",
            archived_at: "2026-08-02T00:00:00Z",
            created_at: "2026-08-01T00:00:00Z",
          },
        ],
        next_cursor: {
          archived_at: "2026-08-02T00:00:00Z",
          branch_id: "branch-2",
        },
      }}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Restore" }));

  await waitFor(() => expect(changeRetention).toHaveBeenCalledTimes(1));
  expect(changeRetention).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-2",
    requestId: "work-branch:restore:00000000-0000-4000-8000-000000000001",
    expectedWorkRevision: 3,
    expectedBranchRevision: 2,
    kind: "restore",
  });
  expect(push).toHaveBeenCalledWith("/works/work-1?branch=branch-2");
  fireEvent.click(screen.getByRole("button", { name: "Show more archived" }));
  expect(await screen.findByText("Archived Aug 1, 2026")).toBeVisible();
  expect(loadArchivedBranches).toHaveBeenCalledWith({
    workId: "work-1",
    before: {
      archived_at: "2026-08-02T00:00:00Z",
      branch_id: "branch-2",
    },
  });
});

test("requires inline confirmation and removes an archived approach only after terminal deletion", async () => {
  const value = snapshot();
  const branchProps = mainBranchProps(value);
  deleteBranch.mockResolvedValue({
    ok: true,
    operation: {
      schema_version: 1,
      operation_id: "deletion-1",
      work_id: "work-1",
      branch_id: "branch-2",
      state: "succeeded",
      phase: "complete",
      outcome: "deleted",
      work_revision: 4,
      branch_revision: 3,
      created_at: "2026-08-02T00:01:00Z",
      completed_at: "2026-08-02T00:01:01Z",
    },
  });
  render(
    <WorkOverviewPage
      initial={value}
      {...branchProps}
      archivedBranches={{
        schema_version: 1,
        work_id: "work-1",
        work_revision: 3,
        branches: [
          {
            branch_id: "branch-2",
            branch_revision: 2,
            origin_branch_id: "branch-1",
            archived_at: "2026-08-02T00:00:00Z",
            created_at: "2026-08-01T00:00:00Z",
          },
        ],
        next_cursor: null,
      }}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Delete" }));
  expect(deleteBranch).not.toHaveBeenCalled();
  expect(screen.getByText(/permanently remove this approach/i)).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

  await waitFor(() => expect(deleteBranch).toHaveBeenCalledTimes(1));
  expect(deleteBranch).toHaveBeenCalledWith({
    workId: "work-1",
    branchId: "branch-2",
    requestId: "work-branch:delete:00000000-0000-4000-8000-000000000001",
    expectedWorkRevision: 3,
    expectedBranchRevision: 2,
  });
  await waitFor(() =>
    expect(screen.queryByText("Archived Aug 2, 2026")).not.toBeInTheDocument(),
  );
  expect(refresh).toHaveBeenCalled();
});

test("keeps a converging deletion visible as typed operation progress", async () => {
  const value = snapshot();
  const branchProps = mainBranchProps(value);
  deleteBranch.mockResolvedValue({
    ok: true,
    operation: {
      schema_version: 1,
      operation_id: "deletion-1",
      work_id: "work-1",
      branch_id: "branch-2",
      state: "pending",
      phase: "session_cleanup",
      outcome: "pending",
      work_revision: 4,
      branch_revision: 3,
      created_at: "2026-08-02T00:01:00Z",
      completed_at: null,
    },
  });
  observeDeletion.mockResolvedValue({
    ok: false,
    status: 503,
    code: "work_branch_deletion_unavailable",
    retryable: true,
  });
  render(
    <WorkOverviewPage
      initial={value}
      {...branchProps}
      archivedBranches={{
        schema_version: 1,
        work_id: "work-1",
        work_revision: 3,
        branches: [
          {
            branch_id: "branch-2",
            branch_revision: 2,
            origin_branch_id: "branch-1",
            archived_at: "2026-08-02T00:00:00Z",
            created_at: "2026-08-01T00:00:00Z",
          },
        ],
        next_cursor: null,
      }}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Delete" }));
  fireEvent.click(screen.getByRole("button", { name: "Delete permanently" }));

  expect(await screen.findByText("Removing session history…")).toBeVisible();
  expect(screen.getByRole("button", { name: "Check progress" })).toBeVisible();
  expect(screen.queryByRole("button", { name: "Restore" })).not.toBeInTheDocument();
});

test("loads deterministic comparison facts only after explicit user intent", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  compareBranches.mockResolvedValue({ ok: true, comparison: comparison() });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  expect(compareBranches).not.toHaveBeenCalled();
  expect(screen.queryByLabelText("Approach comparison")).not.toBeInTheDocument();

  fireEvent.click(
    screen.getByRole("button", { name: "Compare with Main result" }),
  );

  expect(await screen.findByLabelText("Approach comparison")).toBeVisible();
  expect(compareBranches).toHaveBeenCalledWith({
    workId: "work-1",
    leftBranchId: "branch-2",
    rightBranchId: "branch-1",
  });
  expect(screen.getByText("Comparable foundation")).toBeVisible();
  expect(screen.getByText("Same revision · 2 criteria")).toBeVisible();
  expect(screen.getByText("Same · 3 vs 3 items")).toBeVisible();
  expect(
    screen.getByText(
      "Not compared yet: change details, risks, time and cost.",
    ),
  ).toBeVisible();
  expect(screen.getByText(/has not chosen a preferred approach/i)).toBeVisible();
  expect(screen.getByRole("button", { name: "Use this result" })).toBeEnabled();
});

test("uses only the exact compared result and refreshes after the sealed receipt", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  const report = comparison();
  compareBranches.mockResolvedValue({ ok: true, comparison: report });
  selectDelivery.mockResolvedValue({
    ok: true,
    receipt: {
      schema_version: 1,
      work_id: "work-1",
      request_id: "work-delivery:00000000-0000-4000-8000-000000000001",
      delivery_branch_id: "branch-2",
      work_revision: 3,
      branch_revision: 1,
      graph_revision: 1,
      evidence_manifest_hash: report.left_evidence.manifest_hash,
      outcome: "selected",
    },
  });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(screen.getByRole("button", { name: "Compare with Main result" }));
  fireEvent.click(await screen.findByRole("button", { name: "Use this result" }));

  await waitFor(() => expect(selectDelivery).toHaveBeenCalledTimes(1));
  expect(selectDelivery).toHaveBeenCalledWith({
    workId: "work-1",
    requestId: "work-delivery:00000000-0000-4000-8000-000000000001",
    comparison: report,
  });
  expect(refresh).toHaveBeenCalledTimes(1);
  expect(screen.queryByLabelText("Approach comparison")).not.toBeInTheDocument();
});

test("invalidates a stale comparison instead of retrying it against changed facts", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  compareBranches.mockResolvedValue({ ok: true, comparison: comparison() });
  selectDelivery.mockResolvedValue({
    ok: false,
    status: 409,
    code: "work_delivery_selection_conflict",
    retryable: false,
  });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(screen.getByRole("button", { name: "Compare with Main result" }));
  fireEvent.click(await screen.findByRole("button", { name: "Use this result" }));

  expect(await screen.findByText(/changed after it was compared/i)).toBeVisible();
  expect(screen.queryByLabelText("Approach comparison")).not.toBeInTheDocument();
  expect(refresh).toHaveBeenCalledTimes(1);
});

test("retries an unconfirmed selection with the same idempotency identity", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  const report = comparison();
  compareBranches.mockResolvedValue({ ok: true, comparison: report });
  selectDelivery
    .mockResolvedValueOnce({
      ok: false,
      status: 503,
      code: "work_write_unavailable",
      retryable: true,
    })
    .mockResolvedValueOnce({
      ok: true,
      receipt: {
        schema_version: 1,
        work_id: "work-1",
        request_id: "work-delivery:00000000-0000-4000-8000-000000000001",
        delivery_branch_id: "branch-2",
        work_revision: 3,
        branch_revision: 1,
        graph_revision: 1,
        evidence_manifest_hash: report.left_evidence.manifest_hash,
        outcome: "selected",
      },
    });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(screen.getByRole("button", { name: "Compare with Main result" }));
  fireEvent.click(await screen.findByRole("button", { name: "Use this result" }));
  expect(await screen.findByText(/safely try the same request again/i)).toBeVisible();
  fireEvent.click(screen.getByRole("button", { name: "Use this result" }));

  await waitFor(() => expect(selectDelivery).toHaveBeenCalledTimes(2));
  expect(selectDelivery.mock.calls[0]![0].requestId).toBe(
    selectDelivery.mock.calls[1]![0].requestId,
  );
  expect(refresh).toHaveBeenCalledTimes(1);
});

test("shows typed basis incompatibility without ranking either approach", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  const report = comparison({
    directly_comparable: false,
    blockers: ["criteria_revision_differs"],
    left: {
      ...comparison().left,
      criteria: {
        ...comparison().left.criteria,
        revision: 2,
        manifest_hash: `sha256:${"e".repeat(64)}`,
      },
    },
  });
  compareBranches.mockResolvedValue({ ok: true, comparison: report });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(
    screen.getByRole("button", { name: "Compare with Main result" }),
  );

  expect(await screen.findByText("Not directly comparable")).toBeVisible();
  expect(screen.getByText("Different revision")).toBeVisible();
  expect(screen.queryByText(/winner|recommended/i)).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Use this result" })).not.toBeInTheDocument();
});

test("keeps a retryable comparison failure local and retryable", async () => {
  const value = snapshot();
  const branchProps = alternativeBranchProps(value);
  compareBranches.mockResolvedValue({
    ok: false,
    status: 503,
    code: "work_branch_comparison_unavailable",
    retryable: true,
  });
  render(<WorkOverviewPage initial={value} {...branchProps} />);

  fireEvent.click(
    screen.getByRole("button", { name: "Compare with Main result" }),
  );

  expect(await screen.findByText(/temporarily unavailable/i)).toBeVisible();
  expect(
    screen.getByRole("button", { name: "Compare with Main result" }),
  ).toBeEnabled();
  expect(refresh).not.toHaveBeenCalled();
});

test("discards a comparison response after the selected branch changes", async () => {
  const first = snapshot();
  const alternative = alternativeBranchProps(first);
  let resolveComparison!: (value: {
    ok: true;
    comparison: WorkBranchComparisonReportV2;
  }) => void;
  compareBranches.mockReturnValue(
    new Promise((resolve) => {
      resolveComparison = resolve;
    }),
  );
  const { rerender } = render(
    <WorkOverviewPage initial={first} {...alternative} />,
  );
  fireEvent.click(
    screen.getByRole("button", { name: "Compare with Main result" }),
  );

  const second = snapshot();
  const main = mainBranchProps(second);
  rerender(<WorkOverviewPage initial={second} {...main} />);
  resolveComparison({ ok: true, comparison: comparison() });

  await waitFor(() => expect(compareBranches).toHaveBeenCalledTimes(1));
  expect(screen.queryByLabelText("Approach comparison")).not.toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "Compare with Main result" }),
  ).not.toBeInTheDocument();
});

test("keeps pending creation observable and stops the same durable operation", async () => {
  const pending = forkOperation({
    state: "pending",
    outcome: "pending",
    completed_at: null,
  });
  createBranch.mockResolvedValue({ ok: true, operation: pending });
  observeBranch.mockResolvedValue({ ok: true, operation: pending });
  abortBranch.mockResolvedValue({ ok: true });
  render(
    <TestWorkOverviewPage
      initial={snapshot()}
      attachment={committedAttachment()}
    />,
  );

  fireEvent.click(screen.getByRole("button", { name: "Try another approach" }));
  fireEvent.click(await screen.findByRole("button", { name: "Stop creating" }));

  await waitFor(() => expect(abortBranch).toHaveBeenCalledTimes(1));
  expect(abortBranch).toHaveBeenCalledWith({
    workId: "work-1",
    originBranchId: "branch-1",
    operationId: "fork-1",
  });
  expect(screen.queryByRole("button", { name: "Stop creating" })).not.toBeInTheDocument();
});

test("bounds background observation when durable creation remains pending", async () => {
  vi.useFakeTimers();
  try {
    const pending = forkOperation({
      state: "pending",
      outcome: "pending",
      completed_at: null,
    });
    createBranch.mockResolvedValue({ ok: true, operation: pending });
    observeBranch.mockResolvedValue({ ok: true, operation: pending });
    render(
      <TestWorkOverviewPage
        initial={snapshot()}
        attachment={committedAttachment()}
      />,
    );

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Try another approach" }));
    });
    for (const delay of [500, 1_000, 2_000, 4_000, 4_000, 4_000]) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(delay);
      });
    }

    expect(observeBranch).toHaveBeenCalledTimes(6);
    expect(screen.getByText(/taking longer than expected/i)).toBeVisible();
    expect(screen.getByRole("button", { name: "Try another approach" })).toBeEnabled();
  } finally {
    vi.useRealTimers();
  }
});
