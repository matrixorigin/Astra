import type {
  WorkCriteriaPageV1,
  WorkBranchCatalogV1,
  WorkCriteriaProposalListV1,
  WorkEventPageV1,
  WorkObservationReportV1,
  WorkTaskGraphPageV2,
} from "@astra/sdk";
import {
  getWorkBranchPresentation,
  getWorkOverviewSnapshot,
  RequestedWorkBranchNotFound,
} from "@/lib/work-overview";

const report: WorkObservationReportV1 = {
  schema_version: 1,
  report_id: "work-observation:report-1",
  content_hash: "sha256:report",
  scope: "declared_work",
  as_of: {
    work_revision: 2,
    goal_revision: 1,
    criteria_set_revision: 1,
    delivery_branch_revision: 1,
    graph_revision: 1,
    event_head: 3,
  },
  source_revisions: [],
  coherence: "coherent",
  coverage_gaps: [],
  finding: {
    fact_code: "criteria_not_accepted",
    cause_code: "accepted_criteria_empty",
  },
  satisfaction_evidence_refs: [],
  overview: {
    work_id: "work-1",
    work_revision: 2,
    project_id: null,
    original_intent_ref: "intent-1",
    goal: { revision: 1, goal: "Ship a reliable change" },
    criteria: { revision: 1, member_count: 0, manifest_hash: "sha256:criteria" },
    delivery_branch: {
      work_id: "work-1",
      branch_id: "delivery-branch",
      branch_revision: 1,
      origin_branch_id: null,
      fork_cursor: null,
      goal_revision_ref: 1,
      goal_alignment: "current",
      criteria_set_revision_ref: 1,
      criteria_alignment: "current",
      basis_graph_revision: 1,
      current_graph_revision: 1,
      retention_state: "active",
      created_at: "2026-08-01T00:00:00Z",
      archived_at: null,
    },
    graph: { revision: 1, item_count: 1, edge_count: 0, manifest_hash: "sha256:graph" },
    delivery: {
      status: "criteria_not_accepted",
      required_criterion_count: 0,
      satisfied_criterion_count: 0,
      fresh_check_count: 0,
      accepted_gap_count: 0,
      remaining_criterion_count: 0,
      subject_revision: null,
      freshness_valid_until: null,
    },
    event_head: 3,
    retention_state: "active",
    created_at: "2026-08-01T00:00:00Z",
    archived_at: null,
  },
};

const criteria: WorkCriteriaPageV1 = {
  schema_version: 1,
  basis: {
    work_id: "work-1",
    work_revision: 2,
    criteria_set_revision: 1,
    manifest_hash: "sha256:criteria",
    member_count: 0,
  },
  cursor: { criteria_set_revision: 1, offset: 0 },
  next_cursor: null,
  criteria: { offset: 0, limit: 8, total: 0, entries: [] },
};

const proposals: WorkCriteriaProposalListV1 = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "delivery-branch",
  proposals: [],
};

const taskGraph: WorkTaskGraphPageV2 = {
  schema_version: 2,
  scope: "declared_work",
  basis: {
    work_id: "work-1",
    work_revision: 2,
    goal_revision: 1,
    goal: "Ship a reliable change",
    criteria_set_revision: 1,
    criteria_member_count: 0,
    criteria_manifest_hash: "sha256:criteria",
    branch_id: "delivery-branch",
    branch_revision: 1,
    branch_goal_revision: 1,
    branch_criteria_set_revision: 1,
    branch_basis_graph_revision: 1,
    graph_revision: 1,
    graph_item_count: 1,
    graph_edge_count: 0,
    graph_manifest_hash: "sha256:graph",
  },
  cursor: { graph_revision: 1, item_offset: 0, dependency_offset: 0 },
  next_cursor: null,
  items: { offset: 0, limit: 8, total: 0, entries: [] },
  dependencies: { offset: 0, limit: 16, total: 0, entries: [] },
};

const activityMetadata: WorkEventPageV1 = {
  schema_version: 1,
  work_id: "work-1",
  requested_after_event_seq: 3,
  next_after_event_seq: 3,
  event_head: 3,
  retained_from_event_seq: 1,
  seen_through_event_seq: 1,
  coverage: "complete",
  has_more: false,
  events: [],
};

const activityPage: WorkEventPageV1 = {
  ...activityMetadata,
  requested_after_event_seq: 1,
  next_after_event_seq: 3,
  events: [
    {
      event_seq: 2,
      branch_id: "delivery-branch",
      kind: "plan_proposed",
      work_revision: 2,
      goal_revision: 1,
      criterion_set_revision: 1,
      branch_revision: 1,
      graph_revision: 1,
      source_ref: "proposal-1",
      created_at: "2026-08-01T00:01:00Z",
    },
    {
      event_seq: 3,
      branch_id: "delivery-branch",
      kind: "criteria_proposed",
      work_revision: 2,
      goal_revision: 1,
      criterion_set_revision: 1,
      branch_revision: 1,
      graph_revision: 1,
      source_ref: "proposal-2",
      created_at: "2026-08-01T00:02:00Z",
    },
  ],
};

const branchCatalog: WorkBranchCatalogV1 = {
  schema_version: 1,
  work_id: "work-1",
  work_revision: 2,
  delivery_branch_id: "delivery-branch",
  branches: [
    {
      branch_id: "delivery-branch",
      branch_revision: 1,
      is_delivery: true,
      origin_branch_id: null,
      fork_cursor: null,
      goal_revision_ref: 1,
      criteria_set_revision_ref: 1,
      basis_graph_revision: 1,
      current_graph_revision: 1,
      materialization: null,
      created_at: "2026-08-01T00:00:00Z",
    },
    {
      branch_id: "alternative-branch",
      branch_revision: 1,
      is_delivery: false,
      origin_branch_id: "delivery-branch",
      fork_cursor: `sha256:${"a".repeat(64)}`,
      goal_revision_ref: 1,
      criteria_set_revision_ref: 1,
      basis_graph_revision: 1,
      current_graph_revision: 1,
      materialization: [
        { dimension: "conversation", disposition: "shared" },
        { dimension: "goal", disposition: "shared" },
        { dimension: "criteria", disposition: "shared" },
        { dimension: "task_graph", disposition: "shared" },
        { dimension: "checkpoint", disposition: "gap" },
        { dimension: "workspace", disposition: "gap" },
        { dimension: "artifacts", disposition: "gap" },
        { dimension: "transient_authority", disposition: "excluded" },
      ],
      created_at: "2026-08-01T00:01:00Z",
    },
  ],
};

test("first Work paint loads bounded summaries without proposal payloads", async () => {
  const getWorkOverview = vi.fn().mockResolvedValue(report);
  const getWorkCriteria = vi.fn().mockResolvedValue(criteria);
  const listWorkCriteriaProposals = vi.fn().mockResolvedValue(proposals);
  const getWorkTaskGraph = vi.fn().mockResolvedValue(taskGraph);
  const listWorkEvents = vi
    .fn()
    .mockResolvedValueOnce(activityMetadata)
    .mockResolvedValueOnce(activityPage);

  const result = await getWorkOverviewSnapshot(
    {
      getWorkOverview,
      getWorkCriteria,
      listWorkCriteriaProposals,
      getWorkTaskGraph,
      listWorkEvents,
    },
    "work-1",
  );

  expect(result).toEqual({
    report,
    criteria,
    proposals,
    taskGraph,
    activity: {
      eventHead: 3,
      seenThroughEventSeq: 1,
      retainedFromEventSeq: 1,
      unseenCount: 2,
      truncated: false,
      events: activityPage.events,
    },
  });
  expect(getWorkOverview).toHaveBeenCalledWith("work-1");
  expect(getWorkCriteria).toHaveBeenCalledWith("work-1", {
    cursor: { criteria_set_revision: 1, offset: 0 },
    limit: 8,
  });
  expect(listWorkCriteriaProposals).toHaveBeenCalledWith(
    "work-1",
    "delivery-branch",
  );
  expect(getWorkTaskGraph).toHaveBeenCalledWith("work-1", "delivery-branch", {
    cursor: { graph_revision: 1, item_offset: 0, dependency_offset: 0 },
    itemLimit: 8,
    dependencyLimit: 16,
  });
  expect(listWorkEvents).toHaveBeenNthCalledWith(1, "work-1", {
    afterEventSeq: 3,
    limit: 1,
  });
  expect(listWorkEvents).toHaveBeenNthCalledWith(2, "work-1", {
    afterEventSeq: 1,
    limit: 12,
  });
});

test("retries one causally mixed first paint and never returns the mixed projections", async () => {
  const staleCriteria = structuredClone(criteria);
  staleCriteria.basis.work_revision = 3;
  const getWorkOverview = vi.fn().mockResolvedValue(report);
  const getWorkCriteria = vi
    .fn()
    .mockResolvedValueOnce(staleCriteria)
    .mockResolvedValueOnce(criteria);
  const listWorkCriteriaProposals = vi.fn().mockResolvedValue(proposals);
  const getWorkTaskGraph = vi.fn().mockResolvedValue(taskGraph);
  const listWorkEvents = vi
    .fn()
    .mockResolvedValueOnce(activityMetadata)
    .mockResolvedValueOnce(activityMetadata)
    .mockResolvedValueOnce(activityPage);

  const result = await getWorkOverviewSnapshot(
    {
      getWorkOverview,
      getWorkCriteria,
      listWorkCriteriaProposals,
      getWorkTaskGraph,
      listWorkEvents,
    },
    "work-1",
  );

  expect(getWorkOverview).toHaveBeenCalledTimes(2);
  expect(result.criteria.basis.work_revision).toBe(2);
  expect(result.activity.events).toEqual(activityPage.events);
});

test("loads an alternative by exact catalog identity and revision-pinned projections", async () => {
  const alternativeProposals = { ...proposals, branch_id: "alternative-branch" };
  const alternativeGraph = structuredClone(taskGraph);
  alternativeGraph.basis.branch_id = "alternative-branch";
  const getWorkOverview = vi.fn().mockResolvedValue(report);
  const getWorkCriteria = vi.fn().mockResolvedValue(criteria);
  const listWorkCriteriaProposals = vi.fn((_: string, branchId: string) =>
    Promise.resolve(
      branchId === "alternative-branch" ? alternativeProposals : proposals,
    ),
  );
  const getWorkTaskGraph = vi.fn((_: string, branchId: string) =>
    Promise.resolve(branchId === "alternative-branch" ? alternativeGraph : taskGraph),
  );
  const listWorkEvents = vi
    .fn()
    .mockResolvedValueOnce(activityMetadata)
    .mockResolvedValueOnce(activityPage);
  const listWorkBranches = vi.fn().mockResolvedValue(branchCatalog);

  const result = await getWorkBranchPresentation(
    {
      getWorkOverview,
      getWorkCriteria,
      listWorkCriteriaProposals,
      getWorkTaskGraph,
      listWorkEvents,
      listWorkBranches,
    },
    "work-1",
    "alternative-branch",
  );

  expect(result.selectedBranch.branch_id).toBe("alternative-branch");
  expect(result.snapshot.taskGraph).toBe(alternativeGraph);
  expect(result.snapshot.proposals).toBe(alternativeProposals);
  expect(getWorkCriteria).toHaveBeenCalledTimes(1);
  expect(getWorkTaskGraph).toHaveBeenCalledTimes(1);
  expect(getWorkCriteria).toHaveBeenLastCalledWith("work-1", {
    cursor: { criteria_set_revision: 1, offset: 0 },
    limit: 8,
  });
  expect(getWorkTaskGraph).toHaveBeenLastCalledWith(
    "work-1",
    "alternative-branch",
    {
      cursor: { graph_revision: 1, item_offset: 0, dependency_offset: 0 },
      itemLimit: 8,
      dependencyLimit: 16,
    },
  );
});

test("rejects an unknown branch instead of guessing from branch text", async () => {
  const listWorkEvents = vi
    .fn()
    .mockResolvedValueOnce(activityMetadata)
    .mockResolvedValueOnce(activityPage);
  const reader = {
    getWorkOverview: vi.fn().mockResolvedValue(report),
    getWorkCriteria: vi.fn().mockResolvedValue(criteria),
    listWorkCriteriaProposals: vi.fn().mockResolvedValue(proposals),
    getWorkTaskGraph: vi.fn().mockResolvedValue(taskGraph),
    listWorkEvents,
    listWorkBranches: vi.fn().mockResolvedValue(branchCatalog),
  };

  await expect(
    getWorkBranchPresentation(reader, "work-1", "Alternative 1"),
  ).rejects.toBeInstanceOf(RequestedWorkBranchNotFound);
  expect(reader.getWorkTaskGraph).not.toHaveBeenCalled();
});
