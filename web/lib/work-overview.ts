import { AstraApiError } from "@astra/sdk";
import type {
  AstraClient,
  WorkCriteriaPageV1,
  WorkBranchCatalogEntryV1,
  WorkBranchCatalogV1,
  WorkCriteriaProposalListV1,
  WorkEventRecordV1,
  WorkObservationReportV1,
  WorkTaskGraphPageV2,
} from "@astra/sdk";

const WORK_ACTIVITY_WINDOW = 12;

export type WorkOverviewSnapshot = {
  report: WorkObservationReportV1;
  criteria: WorkCriteriaPageV1;
  proposals: WorkCriteriaProposalListV1;
  taskGraph: WorkTaskGraphPageV2;
  activity: WorkEventRecordV1[];
};

type WorkOverviewReader = Pick<
  AstraClient,
  | "getWorkOverview"
  | "getWorkCriteria"
  | "listWorkCriteriaProposals"
  | "getWorkTaskGraph"
  | "listWorkEvents"
>;

type WorkBranchPresentationReader = WorkOverviewReader &
  Pick<AstraClient, "listWorkBranches">;

export type WorkBranchPresentation = {
  snapshot: WorkOverviewSnapshot;
  catalog: WorkBranchCatalogV1;
  selectedBranch: WorkBranchCatalogEntryV1;
};

export class RequestedWorkBranchNotFound extends Error {}

class WorkSnapshotDrift extends Error {}

function assertCurrentProjection(
  report: WorkObservationReportV1,
  criteria: WorkCriteriaPageV1,
  proposals: WorkCriteriaProposalListV1,
  taskGraph: WorkTaskGraphPageV2,
) {
  const overview = report.overview;
  const branch = overview.delivery_branch;
  if (
    criteria.basis.work_id !== overview.work_id ||
    criteria.basis.work_revision !== overview.work_revision ||
    criteria.basis.criteria_set_revision !== overview.criteria.revision ||
    criteria.basis.manifest_hash !== overview.criteria.manifest_hash ||
    proposals.work_id !== overview.work_id ||
    proposals.branch_id !== branch.branch_id ||
    taskGraph.basis.work_id !== overview.work_id ||
    taskGraph.basis.work_revision !== overview.work_revision ||
    taskGraph.basis.goal_revision !== overview.goal.revision ||
    taskGraph.basis.criteria_set_revision !== overview.criteria.revision ||
    taskGraph.basis.branch_id !== branch.branch_id ||
    taskGraph.basis.branch_revision !== branch.branch_revision ||
    taskGraph.basis.graph_revision !== overview.graph.revision ||
    taskGraph.basis.graph_manifest_hash !== overview.graph.manifest_hash
  ) {
    throw new WorkSnapshotDrift("Work projections changed during the bounded read");
  }
}

function assertSelectedBranchProjection(
  report: WorkObservationReportV1,
  branch: WorkBranchCatalogEntryV1,
  criteria: WorkCriteriaPageV1,
  proposals: WorkCriteriaProposalListV1,
  taskGraph: WorkTaskGraphPageV2,
) {
  const overview = report.overview;
  const displayedCriteriaRevision = branch.is_delivery
    ? overview.criteria.revision
    : branch.criteria_set_revision_ref;
  if (
    criteria.basis.work_id !== overview.work_id ||
    criteria.basis.work_revision !== overview.work_revision ||
    criteria.basis.criteria_set_revision !== displayedCriteriaRevision ||
    proposals.work_id !== overview.work_id ||
    proposals.branch_id !== branch.branch_id ||
    taskGraph.basis.work_id !== overview.work_id ||
    taskGraph.basis.work_revision !== overview.work_revision ||
    taskGraph.basis.goal_revision !== overview.goal.revision ||
    taskGraph.basis.criteria_set_revision !== overview.criteria.revision ||
    taskGraph.basis.branch_id !== branch.branch_id ||
    taskGraph.basis.branch_revision !== branch.branch_revision ||
    taskGraph.basis.branch_goal_revision !== branch.goal_revision_ref ||
    taskGraph.basis.branch_criteria_set_revision !== branch.criteria_set_revision_ref ||
    taskGraph.basis.branch_basis_graph_revision !== branch.basis_graph_revision ||
    taskGraph.basis.graph_revision !== branch.current_graph_revision
  ) {
    throw new WorkSnapshotDrift("selected branch changed during the bounded read");
  }
}

async function loadSnapshotOnce(
  sdk: WorkOverviewReader,
  workId: string,
  observedReport?: WorkObservationReportV1,
  selectedBranch?: WorkBranchCatalogEntryV1,
): Promise<WorkOverviewSnapshot> {
  const report = observedReport ?? (await sdk.getWorkOverview(workId));
  const overview = report.overview;
  const branchId = selectedBranch?.branch_id ?? overview.delivery_branch.branch_id;
  const criteriaRevision =
    selectedBranch && !selectedBranch.is_delivery
      ? selectedBranch.criteria_set_revision_ref
      : overview.criteria.revision;
  const graphRevision = selectedBranch?.current_graph_revision ?? overview.graph.revision;
  const activityAfter = overview.event_head - WORK_ACTIVITY_WINDOW;
  const [criteria, proposals, taskGraph, activityPage] = await Promise.all([
    sdk.getWorkCriteria(workId, {
      cursor: {
        criteria_set_revision: criteriaRevision,
        offset: 0,
      },
      limit: 8,
    }),
    sdk.listWorkCriteriaProposals(workId, branchId),
    sdk.getWorkTaskGraph(workId, branchId, {
      cursor: {
        graph_revision: graphRevision,
        item_offset: 0,
        dependency_offset: 0,
      },
      itemLimit: 8,
      dependencyLimit: 16,
    }),
    sdk.listWorkEvents(workId, {
      ...(activityAfter > 0 ? { afterEventSeq: activityAfter } : {}),
      limit: WORK_ACTIVITY_WINDOW,
    }),
  ]);

  if (selectedBranch) {
    assertSelectedBranchProjection(report, selectedBranch, criteria, proposals, taskGraph);
  } else {
    assertCurrentProjection(report, criteria, proposals, taskGraph);
  }
  if (
    activityPage.work_id !== workId ||
    activityPage.requested_after_event_seq !== (activityAfter > 0 ? activityAfter : null) ||
    activityPage.event_head !== overview.event_head ||
    activityPage.has_more
  ) {
    throw new WorkSnapshotDrift("Work events changed during the bounded read");
  }

  return { report, criteria, proposals, taskGraph, activity: activityPage.events };
}

/**
 * Load only the bounded data needed for the first Work paint. Proposal payloads,
 * task history, transcript, and evidence remain lazy projections.
 */
export async function getWorkOverviewSnapshot(
  sdk: WorkOverviewReader,
  workId: string,
): Promise<WorkOverviewSnapshot> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      return await loadSnapshotOnce(sdk, workId);
    } catch (error) {
      const staleApiRead =
        error instanceof AstraApiError &&
        (error.status === 409 || error.status === 412);
      if (attempt === 0 && (error instanceof WorkSnapshotDrift || staleApiRead)) {
        continue;
      }
      throw error;
    }
  }
  throw new Error("unreachable bounded Work snapshot retry");
}

/** Resolve one exact active branch and load only its bounded first-paint projections. */
export async function getWorkBranchPresentation(
  sdk: WorkBranchPresentationReader,
  workId: string,
  requestedBranchId?: string,
): Promise<WorkBranchPresentation> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      const [report, catalog] = await Promise.all([
        sdk.getWorkOverview(workId),
        sdk.listWorkBranches(workId),
      ]);
      const overview = report.overview;
      if (
        catalog.work_id !== overview.work_id ||
        catalog.work_revision !== overview.work_revision ||
        catalog.delivery_branch_id !== overview.delivery_branch.branch_id
      ) {
        throw new WorkSnapshotDrift("Work branch catalog changed during the bounded read");
      }
      const selectedBranch = catalog.branches.find(
        (branch) => branch.branch_id === (requestedBranchId ?? catalog.delivery_branch_id),
      );
      if (!selectedBranch) {
        throw new RequestedWorkBranchNotFound("requested Work branch is not active");
      }
      return {
        snapshot: await loadSnapshotOnce(sdk, workId, report, selectedBranch),
        catalog,
        selectedBranch,
      };
    } catch (error) {
      if (error instanceof RequestedWorkBranchNotFound) throw error;
      const staleApiRead =
        error instanceof AstraApiError && (error.status === 409 || error.status === 412);
      if (attempt === 0 && (error instanceof WorkSnapshotDrift || staleApiRead)) continue;
      throw error;
    }
  }
  throw new Error("unreachable bounded Work branch presentation retry");
}
