import type {
  WorkBranchOverviewV1,
  WorkArchivedBranchPageV1,
  WorkContentHash,
  WorkCriteriaSummaryV1,
  WorkGraphSummaryV1,
  WorkObservationCoverageGapV1,
  WorkObservationCursorV1,
  WorkObservationSatisfactionEvidenceRefV1,
  WorkObservationFindingV1,
  WorkObservationReportV1,
  WorkObservationSourceKind,
  WorkObservationSourceRevisionV1,
  WorkOverviewV1,
  WorkCatalogCursorV1,
  WorkCatalogEntryV1,
  WorkCatalogPageV1,
  WorkBranchAttachmentV1,
  WorkBranchControlBasisV1,
  WorkBranchControlOperationV2,
  WorkBranchCreationOperationV1,
  WorkBranchDeletionOperationV1,
  WorkBranchRetentionReceiptV1,
  WorkBranchCatalogEntryV1,
  WorkBranchCatalogV1,
  WorkBranchComparisonReportV2,
  WorkPatchArtifactCursorV1,
  WorkPatchArtifactPageV1,
  WorkPatchArtifactV1,
  WorkPatchMaterializationOperationV2,
  WorkPatchMaterializationPageV2,
  WorkPatchCommitOperationV1,
  WorkPatchCommitPageV1,
  WorkDeliverySelectionReceiptV1,
  WorkConversationHeadV1,
  WorkTranscriptItemV1,
  WorkTranscriptPageV1,
  WorkEventKind,
  WorkEventPageV1,
  WorkEventRecordV1,
  WorkReadCursorReceiptV1,
  WorkCriteriaCursorV1,
  WorkCriteriaPageV1,
  WorkCriterionV1,
  WorkCriteriaProposalBasisV1,
  WorkCriteriaProposalDetailV1,
  WorkCriteriaProposalListV1,
  WorkCriteriaProposalMemberV1,
  WorkCriteriaProposalResolutionV1,
  WorkCriteriaProposalSummaryV1,
  WorkProposalStatus,
  WorkTaskGraphBasisV1,
  WorkTaskGraphCursorV1,
  WorkTaskGraphDependencyV1,
  WorkTaskGraphItemV2,
  WorkTaskGraphPageV2,
  WorkSessionBindingV1,
  StreamEventType,
  WorkTurnStreamEvent,
} from "./types";

/** Strict decoder for the constant-size session-to-Work bootstrap projection. */
export function decodeWorkSessionBindingV1(value: unknown): WorkSessionBindingV1 {
  const path = "work_session_binding";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "branch_id", "graph_revision"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path} has an unsupported schema`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    graph_revision: positiveRevision(
      object.graph_revision,
      `${path}.graph_revision`,
    ),
  };
}

type WireObject = Record<string, unknown>;

function exactObject(
  value: unknown,
  expectedKeys: readonly string[],
  path: string,
): WireObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`);
  }
  const object = value as WireObject;
  const actual = Object.keys(object).sort();
  const expected = [...expectedKeys].sort();
  if (
    actual.length !== expected.length ||
    actual.some((key, index) => key !== expected[index])
  ) {
    throw new TypeError(`${path} has an unsupported field set`);
  }
  return object;
}

function nonEmptyString(value: unknown, path: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new TypeError(`${path} must be a non-empty string`);
  }
  return value;
}

function opaqueIdentity(value: unknown, path: string): string {
  const parsed = nonEmptyString(value, path);
  if (/\s|[\u0000-\u001f\u007f]/u.test(parsed)) {
    throw new TypeError(`${path} is not a canonical identity`);
  }
  return parsed;
}

function resourceIdentity(value: unknown, path: string): string {
  const parsed = opaqueIdentity(value, path);
  if (
    parsed === "." ||
    parsed === ".." ||
    Array.from(parsed).length > 64 ||
    !/^[A-Za-z0-9._-]+$/u.test(parsed)
  ) {
    throw new TypeError(`${path} is not a canonical resource identity`);
  }
  return parsed;
}

function capabilityIdentity(value: unknown, path: string): string {
  const parsed = opaqueIdentity(value, path);
  if (
    parsed === "." ||
    parsed === ".." ||
    Array.from(parsed).length > 128 ||
    !/^[A-Za-z0-9._-]+$/u.test(parsed)
  ) {
    throw new TypeError(`${path} is not a canonical capability identity`);
  }
  return parsed;
}

function positiveRevision(value: unknown, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 1) {
    throw new TypeError(`${path} must be a positive safe integer`);
  }
  return Number(value);
}

function boundedCount(value: unknown, maximum: number, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < 0 || Number(value) > maximum) {
    throw new TypeError(`${path} must be an integer between 0 and ${maximum}`);
  }
  return Number(value);
}

function safeIntegerAtLeast(value: unknown, minimum: number, path: string): number {
  if (!Number.isSafeInteger(value) || Number(value) < minimum) {
    throw new TypeError(`${path} must be a safe integer greater than or equal to ${minimum}`);
  }
  return Number(value);
}

function contentHash(value: unknown, path: string): WorkContentHash {
  const parsed = nonEmptyString(value, path);
  if (!/^sha256:[0-9a-f]{64}$/u.test(parsed)) {
    throw new TypeError(`${path} must be a canonical SHA-256 content hash`);
  }
  return parsed as WorkContentHash;
}

function timestamp(value: unknown, path: string): string {
  const parsed = nonEmptyString(value, path);
  if (
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u.test(parsed) ||
    !Number.isFinite(Date.parse(parsed))
  ) {
    throw new TypeError(`${path} must be an RFC 3339 UTC timestamp`);
  }
  return parsed;
}

function utcTimestampSortKey(value: string): string {
  const match = value.match(
    /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d+))?Z$/u,
  );
  if (!match) throw new TypeError("timestamp was not validated before ordering");
  return `${match[1]}.${(match[2] ?? "").padEnd(9, "0")}`;
}

function nullableTimestamp(value: unknown, path: string): string | null {
  return value === null ? null : timestamp(value, path);
}

function validateRetention(
  state: "active" | "archived",
  createdAt: string,
  archivedAt: string | null,
  path: string,
): void {
  if ((state === "archived") !== (archivedAt !== null)) {
    throw new TypeError(`${path} retention state and archive timestamp disagree`);
  }
  if (archivedAt !== null && Date.parse(archivedAt) < Date.parse(createdAt)) {
    throw new TypeError(`${path} archive timestamp precedes creation`);
  }
}

function oneOf<T extends string>(
  value: unknown,
  allowed: readonly T[],
  path: string,
): T {
  if (typeof value !== "string" || !allowed.includes(value as T)) {
    throw new TypeError(`${path} has an unsupported value`);
  }
  return value as T;
}

function decodeCursor(value: unknown): WorkObservationCursorV1 {
  const object = exactObject(
    value,
    [
      "work_revision",
      "goal_revision",
      "criteria_set_revision",
      "delivery_branch_revision",
      "graph_revision",
      "event_head",
    ],
    "report.as_of",
  );
  return {
    work_revision: positiveRevision(object.work_revision, "report.as_of.work_revision"),
    goal_revision: positiveRevision(object.goal_revision, "report.as_of.goal_revision"),
    criteria_set_revision: positiveRevision(
      object.criteria_set_revision,
      "report.as_of.criteria_set_revision",
    ),
    delivery_branch_revision: positiveRevision(
      object.delivery_branch_revision,
      "report.as_of.delivery_branch_revision",
    ),
    graph_revision: positiveRevision(object.graph_revision, "report.as_of.graph_revision"),
    event_head: positiveRevision(object.event_head, "report.as_of.event_head"),
  };
}

const REVISION_SOURCE_KINDS = [
  "work",
  "goal",
  "criterion_set",
  "delivery_branch",
  "graph",
] as const;

const SOURCE_KINDS = [
  ...REVISION_SOURCE_KINDS,
  "work_events",
] as const;

function decodeSourceRevision(
  value: unknown,
  index: number,
): WorkObservationSourceRevisionV1 {
  const path = `report.source_revisions[${index}]`;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`);
  }
  const source = oneOf(
    (value as WireObject).source,
    SOURCE_KINDS,
    `${path}.source`,
  );
  if (source === "criterion_set" || source === "graph") {
    const object = exactObject(value, ["source", "revision", "content_hash"], path);
    return {
      source,
      revision: positiveRevision(object.revision, `${path}.revision`),
      content_hash: contentHash(object.content_hash, `${path}.content_hash`),
    };
  }
  if (source === "work_events") {
    const object = exactObject(value, ["source", "event_head"], path);
    return {
      source,
      event_head: positiveRevision(object.event_head, `${path}.event_head`),
    };
  }
  const object = exactObject(value, ["source", "revision"], path);
  return {
    source,
    revision: positiveRevision(object.revision, `${path}.revision`),
  };
}

function decodeCoverageGap(
  value: unknown,
  index: number,
): WorkObservationCoverageGapV1 {
  const path = `report.coverage_gaps[${index}]`;
  const object = exactObject(value, ["source", "reason"], path);
  return {
    source: oneOf(object.source, SOURCE_KINDS, `${path}.source`),
    reason: oneOf(
      object.reason,
      ["source_unavailable_at_causal_cut"] as const,
      `${path}.reason`,
    ),
  };
}

function decodeCriteria(value: unknown): WorkCriteriaSummaryV1 {
  const object = exactObject(
    value,
    ["revision", "member_count", "manifest_hash"],
    "report.overview.criteria",
  );
  return {
    revision: positiveRevision(object.revision, "report.overview.criteria.revision"),
    member_count: boundedCount(
      object.member_count,
      128,
      "report.overview.criteria.member_count",
    ),
    manifest_hash: contentHash(
      object.manifest_hash,
      "report.overview.criteria.manifest_hash",
    ),
  };
}

function decodeBranch(value: unknown): WorkBranchOverviewV1 {
  const path = "report.overview.delivery_branch";
  const object = exactObject(
    value,
    [
      "work_id",
      "branch_id",
      "branch_revision",
      "origin_branch_id",
      "fork_cursor",
      "goal_revision_ref",
      "goal_alignment",
      "criteria_set_revision_ref",
      "criteria_alignment",
      "basis_graph_revision",
      "current_graph_revision",
      "retention_state",
      "created_at",
      "archived_at",
    ],
    path,
  );
  const originBranchId =
    object.origin_branch_id === null
      ? null
      : resourceIdentity(object.origin_branch_id, `${path}.origin_branch_id`);
  const forkCursor =
    object.fork_cursor === null
      ? null
      : opaqueIdentity(object.fork_cursor, `${path}.fork_cursor`);
  if ((originBranchId === null) !== (forkCursor === null)) {
    throw new TypeError(`${path} has incomplete fork lineage`);
  }
  const basisGraphRevision = positiveRevision(
    object.basis_graph_revision,
    `${path}.basis_graph_revision`,
  );
  const currentGraphRevision = positiveRevision(
    object.current_graph_revision,
    `${path}.current_graph_revision`,
  );
  if (currentGraphRevision < basisGraphRevision) {
    throw new TypeError(`${path} graph head precedes its basis`);
  }
  const branch: WorkBranchOverviewV1 = {
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    origin_branch_id: originBranchId,
    fork_cursor: forkCursor,
    goal_revision_ref: positiveRevision(object.goal_revision_ref, `${path}.goal_revision_ref`),
    goal_alignment: oneOf(
      object.goal_alignment,
      ["current", "behind"] as const,
      `${path}.goal_alignment`,
    ),
    criteria_set_revision_ref: positiveRevision(
      object.criteria_set_revision_ref,
      `${path}.criteria_set_revision_ref`,
    ),
    criteria_alignment: oneOf(
      object.criteria_alignment,
      ["current", "behind"] as const,
      `${path}.criteria_alignment`,
    ),
    basis_graph_revision: basisGraphRevision,
    current_graph_revision: currentGraphRevision,
    retention_state: oneOf(
      object.retention_state,
      ["active", "archived"] as const,
      `${path}.retention_state`,
    ),
    created_at: timestamp(object.created_at, `${path}.created_at`),
    archived_at: nullableTimestamp(object.archived_at, `${path}.archived_at`),
  };
  validateRetention(
    branch.retention_state,
    branch.created_at,
    branch.archived_at,
    path,
  );
  return branch;
}

function decodeGraph(value: unknown): WorkGraphSummaryV1 {
  const path = "report.overview.graph";
  const object = exactObject(
    value,
    ["revision", "item_count", "edge_count", "manifest_hash"],
    path,
  );
  const itemCount = boundedCount(object.item_count, 256, `${path}.item_count`);
  const edgeCount = boundedCount(object.edge_count, 1024, `${path}.edge_count`);
  if (edgeCount > (itemCount * (itemCount - 1)) / 2) {
    throw new TypeError(`${path}.edge_count exceeds a simple DAG`);
  }
  return {
    revision: positiveRevision(object.revision, `${path}.revision`),
    item_count: itemCount,
    edge_count: edgeCount,
    manifest_hash: contentHash(object.manifest_hash, `${path}.manifest_hash`),
  };
}

function decodeDelivery(
  value: unknown,
  criteria: WorkCriteriaSummaryV1,
  branch: WorkBranchOverviewV1,
): WorkOverviewV1["delivery"] {
  const path = "report.overview.delivery";
  const object = exactObject(
    value,
    [
      "status",
      "required_criterion_count",
      "satisfied_criterion_count",
      "fresh_check_count",
      "accepted_gap_count",
      "remaining_criterion_count",
      "subject_revision",
      "freshness_valid_until",
    ],
    path,
  );
  const status = oneOf(
    object.status,
    [
      "criteria_not_accepted",
      "branch_basis_out_of_date",
      "subject_unavailable",
      "verification_required",
      "ready_for_review",
    ] as const,
    `${path}.status`,
  );
  const required = boundedCount(
    object.required_criterion_count,
    128,
    `${path}.required_criterion_count`,
  );
  const satisfied = boundedCount(
    object.satisfied_criterion_count,
    required,
    `${path}.satisfied_criterion_count`,
  );
  const freshChecks = boundedCount(
    object.fresh_check_count,
    required,
    `${path}.fresh_check_count`,
  );
  const acceptedGaps = boundedCount(
    object.accepted_gap_count,
    required,
    `${path}.accepted_gap_count`,
  );
  const remaining = boundedCount(
    object.remaining_criterion_count,
    required,
    `${path}.remaining_criterion_count`,
  );
  const subjectRevision =
    object.subject_revision === null
      ? null
      : contentHash(object.subject_revision, `${path}.subject_revision`);
  const freshnessValidUntil = nullableTimestamp(
    object.freshness_valid_until,
    `${path}.freshness_valid_until`,
  );
  if (
    required !== criteria.member_count ||
    satisfied !== freshChecks + acceptedGaps ||
    remaining !== required - satisfied
  ) {
    throw new TypeError(`${path} criterion counts are incoherent`);
  }
  const basisCurrent =
    branch.goal_alignment === "current" && branch.criteria_alignment === "current";
  const preVerification =
    status === "criteria_not_accepted" ||
    status === "branch_basis_out_of_date" ||
    status === "subject_unavailable";
  if (
    (status === "criteria_not_accepted") !== (required === 0) ||
    (status === "branch_basis_out_of_date") !== (required > 0 && !basisCurrent) ||
    (status === "subject_unavailable" && (required === 0 || !basisCurrent)) ||
    ((status === "verification_required" || status === "ready_for_review") &&
      (required === 0 || !basisCurrent || subjectRevision === null)) ||
    (status === "verification_required" && remaining === 0) ||
    (status === "ready_for_review" && remaining !== 0) ||
    (freshnessValidUntil !== null && freshChecks === 0) ||
    (preVerification &&
      (subjectRevision !== null || satisfied !== 0 || freshChecks !== 0 || acceptedGaps !== 0))
  ) {
    throw new TypeError(`${path} status disagrees with its exact delivery basis`);
  }
  return {
    status,
    required_criterion_count: required,
    satisfied_criterion_count: satisfied,
    fresh_check_count: freshChecks,
    accepted_gap_count: acceptedGaps,
    remaining_criterion_count: remaining,
    subject_revision: subjectRevision,
    freshness_valid_until: freshnessValidUntil,
  };
}

function decodeOverview(value: unknown): WorkOverviewV1 {
  const path = "report.overview";
  const object = exactObject(
    value,
    [
      "work_id",
      "work_revision",
      "project_id",
      "original_intent_ref",
      "goal",
      "criteria",
      "delivery_branch",
      "graph",
      "delivery",
      "event_head",
      "retention_state",
      "created_at",
      "archived_at",
    ],
    path,
  );
  const goal = exactObject(object.goal, ["revision", "goal"], `${path}.goal`);
  const criteria = decodeCriteria(object.criteria);
  const deliveryBranch = decodeBranch(object.delivery_branch);
  const overview: WorkOverviewV1 = {
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    project_id:
      object.project_id === null
        ? null
        : opaqueIdentity(object.project_id, `${path}.project_id`),
    original_intent_ref: opaqueIdentity(
      object.original_intent_ref,
      `${path}.original_intent_ref`,
    ),
    goal: {
      revision: positiveRevision(goal.revision, `${path}.goal.revision`),
      goal: nonEmptyString(goal.goal, `${path}.goal.goal`),
    },
    criteria,
    delivery_branch: deliveryBranch,
    graph: decodeGraph(object.graph),
    delivery: decodeDelivery(object.delivery, criteria, deliveryBranch),
    event_head: positiveRevision(object.event_head, `${path}.event_head`),
    retention_state: oneOf(
      object.retention_state,
      ["active", "archived"] as const,
      `${path}.retention_state`,
    ),
    created_at: timestamp(object.created_at, `${path}.created_at`),
    archived_at: nullableTimestamp(object.archived_at, `${path}.archived_at`),
  };
  if (overview.delivery_branch.work_id !== overview.work_id) {
    throw new TypeError(`${path}.delivery_branch belongs to a different Work`);
  }
  const expectedGoalAlignment =
    overview.delivery_branch.goal_revision_ref === overview.goal.revision
      ? "current"
      : "behind";
  if (
    overview.delivery_branch.goal_revision_ref > overview.goal.revision ||
    overview.delivery_branch.goal_alignment !== expectedGoalAlignment
  ) {
    throw new TypeError(`${path}.delivery_branch has incoherent Goal alignment`);
  }
  const expectedCriteriaAlignment =
    overview.delivery_branch.criteria_set_revision_ref === overview.criteria.revision
      ? "current"
      : "behind";
  if (
    overview.delivery_branch.criteria_set_revision_ref > overview.criteria.revision ||
    overview.delivery_branch.criteria_alignment !== expectedCriteriaAlignment
  ) {
    throw new TypeError(`${path}.delivery_branch has incoherent criteria alignment`);
  }
  if (overview.delivery_branch.current_graph_revision !== overview.graph.revision) {
    throw new TypeError(`${path}.graph does not match the delivery branch head`);
  }
  validateRetention(
    overview.retention_state,
    overview.created_at,
    overview.archived_at,
    path,
  );
  return overview;
}

function decodeObservationFinding(value: unknown): WorkObservationFindingV1 {
  const path = "report.finding";
  const object = exactObject(value, ["fact_code", "cause_code"], path);
  return {
    fact_code: oneOf(
      object.fact_code,
      [
        "criteria_not_accepted",
        "branch_basis_out_of_date",
        "subject_unavailable",
        "verification_required",
        "ready_for_review",
      ] as const,
      `${path}.fact_code`,
    ),
    cause_code: oneOf(
      object.cause_code,
      [
        "accepted_criteria_empty",
        "branch_basis_stale",
        "current_subject_missing",
        "current_evidence_incomplete",
        "current_evidence_complete",
      ] as const,
      `${path}.cause_code`,
    ),
  };
}

function decodeObservationSatisfactionEvidenceRef(
  value: unknown,
  index: number,
): WorkObservationSatisfactionEvidenceRefV1 {
  const path = `report.satisfaction_evidence_refs[${index}]`;
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`);
  }
  const kind = oneOf(
    (value as Record<string, unknown>).kind,
    ["check_run", "acceptance_decision"] as const,
    `${path}.kind`,
  );
  const fields =
    kind === "check_run"
      ? ["kind", "criterion", "check_run_id", "payload_hash"]
      : ["kind", "criterion", "decision_id", "payload_hash"];
  const object = exactObject(value, fields, path);
  const criterion = exactObject(
    object.criterion,
    ["criterion_id", "revision"],
    `${path}.criterion`,
  );
  const basis = {
    criterion: {
      criterion_id: resourceIdentity(
        criterion.criterion_id,
        `${path}.criterion.criterion_id`,
      ),
      revision: positiveRevision(criterion.revision, `${path}.criterion.revision`),
    },
    payload_hash: contentHash(object.payload_hash, `${path}.payload_hash`),
  };
  return kind === "check_run"
    ? {
        kind,
        ...basis,
        check_run_id: resourceIdentity(object.check_run_id, `${path}.check_run_id`),
      }
    : {
        kind,
        ...basis,
        decision_id: resourceIdentity(object.decision_id, `${path}.decision_id`),
      };
}

/** Strict decoder for the exact Work read major. Unknown or incoherent fields fail closed. */
export function decodeWorkObservationReportV1(value: unknown): WorkObservationReportV1 {
  const object = exactObject(
    value,
    [
      "schema_version",
      "report_id",
      "content_hash",
      "scope",
      "as_of",
      "source_revisions",
      "coherence",
      "coverage_gaps",
      "finding",
      "satisfaction_evidence_refs",
      "overview",
    ],
    "report",
  );
  if (object.schema_version !== 1) {
    throw new TypeError("report.schema_version is unsupported");
  }
  const reportId = nonEmptyString(object.report_id, "report.report_id");
  const reportHash = contentHash(object.content_hash, "report.content_hash");
  if (reportId !== `work-observation:${reportHash.slice("sha256:".length)}`) {
    throw new TypeError("report identity and content hash disagree");
  }
  const asOf = decodeCursor(object.as_of);
  if (
    !Array.isArray(object.source_revisions) ||
    object.source_revisions.length !== SOURCE_KINDS.length
  ) {
    throw new TypeError("report.source_revisions must contain every declared-Work source");
  }
  const sourceRevisions = object.source_revisions.map(decodeSourceRevision);
  const sourceByKind = new Map(
    sourceRevisions.map((source) => [source.source, source] as const),
  );
  if (sourceByKind.size !== SOURCE_KINDS.length) {
    throw new TypeError("report.source_revisions repeats or omits a source");
  }
  if (!Array.isArray(object.coverage_gaps) || object.coverage_gaps.length > SOURCE_KINDS.length) {
    throw new TypeError("report.coverage_gaps is not bounded");
  }
  const coverageGaps = object.coverage_gaps.map(decodeCoverageGap);
  if (new Set(coverageGaps.map((gap) => gap.source)).size !== coverageGaps.length) {
    throw new TypeError("report.coverage_gaps repeats a source");
  }
  const overview = decodeOverview(object.overview);
  const coherence = oneOf(object.coherence, ["coherent"] as const, "report.coherence");
  if (coherence === "coherent" && coverageGaps.length !== 0) {
    throw new TypeError("report coherent causal cut cannot contain coverage gaps");
  }
  const finding = decodeObservationFinding(object.finding);
  const expectedFindingByStatus = {
    criteria_not_accepted: ["criteria_not_accepted", "accepted_criteria_empty"],
    branch_basis_out_of_date: ["branch_basis_out_of_date", "branch_basis_stale"],
    subject_unavailable: ["subject_unavailable", "current_subject_missing"],
    verification_required: ["verification_required", "current_evidence_incomplete"],
    ready_for_review: ["ready_for_review", "current_evidence_complete"],
  } as const;
  const expectedFinding = expectedFindingByStatus[overview.delivery.status];
  if (
    finding.fact_code !== expectedFinding[0] ||
    finding.cause_code !== expectedFinding[1]
  ) {
    throw new TypeError("report finding disagrees with its deterministic delivery facts");
  }
  if (
    !Array.isArray(object.satisfaction_evidence_refs) ||
    object.satisfaction_evidence_refs.length > 128
  ) {
    throw new TypeError("report.satisfaction_evidence_refs is not bounded");
  }
  const satisfactionEvidenceRefs = object.satisfaction_evidence_refs.map(
    decodeObservationSatisfactionEvidenceRef,
  );
  const evidenceCriteria = new Set(
    satisfactionEvidenceRefs.map(
      (evidence) =>
        `${evidence.criterion.criterion_id}@${evidence.criterion.revision}`,
    ),
  );
  const checkCount = satisfactionEvidenceRefs.filter(
    (evidence) => evidence.kind === "check_run",
  ).length;
  const acceptanceCount = satisfactionEvidenceRefs.length - checkCount;
  if (
    evidenceCriteria.size !== satisfactionEvidenceRefs.length ||
    satisfactionEvidenceRefs.length !== overview.delivery.satisfied_criterion_count ||
    checkCount !== overview.delivery.fresh_check_count ||
    acceptanceCount !== overview.delivery.accepted_gap_count
  ) {
    throw new TypeError(
      "report satisfaction evidence refs disagree with delivery coverage",
    );
  }

  const expectedRevisions = {
    work: asOf.work_revision,
    goal: asOf.goal_revision,
    criterion_set: asOf.criteria_set_revision,
    delivery_branch: asOf.delivery_branch_revision,
    graph: asOf.graph_revision,
  };
  for (const kind of REVISION_SOURCE_KINDS) {
    const source = sourceByKind.get(kind);
    if (!source || !("revision" in source) || source.revision !== expectedRevisions[kind]) {
      throw new TypeError(`report source ${kind} disagrees with the causal cursor`);
    }
  }
  const eventSource = sourceByKind.get("work_events");
  if (!eventSource || !("event_head" in eventSource) || eventSource.event_head !== asOf.event_head) {
    throw new TypeError("report source work_events disagrees with the causal cursor");
  }
  if (
    overview.work_revision !== asOf.work_revision ||
    overview.goal.revision !== asOf.goal_revision ||
    overview.criteria.revision !== asOf.criteria_set_revision ||
    overview.delivery_branch.branch_revision !== asOf.delivery_branch_revision ||
    overview.graph.revision !== asOf.graph_revision ||
    overview.event_head !== asOf.event_head
  ) {
    throw new TypeError("report overview disagrees with the causal cursor");
  }
  const criterionSource = sourceByKind.get("criterion_set");
  const graphSource = sourceByKind.get("graph");
  if (
    !criterionSource ||
    !("content_hash" in criterionSource) ||
    criterionSource.content_hash !== overview.criteria.manifest_hash ||
    !graphSource ||
    !("content_hash" in graphSource) ||
    graphSource.content_hash !== overview.graph.manifest_hash
  ) {
    throw new TypeError("report content-addressed sources disagree with the overview");
  }

  return {
    schema_version: 1,
    report_id: reportId as `work-observation:${string}`,
    content_hash: reportHash,
    scope: oneOf(object.scope, ["declared_work"] as const, "report.scope"),
    as_of: asOf,
    source_revisions: sourceRevisions,
    coherence,
    coverage_gaps: coverageGaps,
    finding,
    satisfaction_evidence_refs: satisfactionEvidenceRefs,
    overview,
  };
}

/** Strict decoder for the monotonic Work read-cursor receipt. */
export function decodeWorkReadCursorReceiptV1(
  value: unknown,
): WorkReadCursorReceiptV1 {
  const path = "work_read_cursor_receipt";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "through_event_seq",
      "receipt_revision",
      "receipt_hash",
      "updated_at",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    through_event_seq: positiveRevision(
      object.through_event_seq,
      `${path}.through_event_seq`,
    ),
    receipt_revision: positiveRevision(
      object.receipt_revision,
      `${path}.receipt_revision`,
    ),
    receipt_hash: contentHash(object.receipt_hash, `${path}.receipt_hash`),
    updated_at: timestamp(object.updated_at, `${path}.updated_at`),
  };
}

const EVENT_KINDS = [
  "work_created",
  "goal_revised",
  "criteria_accepted",
  "branch_basis_adopted",
  "graph_replaced",
  "delivery_branch_selected",
  "branch_archived",
  "branch_restored",
  "subject_changed",
  "patch_artifact_exported",
  "plan_proposed",
  "criteria_proposed",
  "proposal_rejected",
  "check_recorded",
  "gaps_accepted",
  "run_completed",
  "run_delegated",
  "run_failed",
  "run_cancelled",
  "runtime_events_expired",
] as const satisfies readonly WorkEventKind[];

function nullableRevision(value: unknown, path: string): number | null {
  return value === null ? null : positiveRevision(value, path);
}

function decodeWorkEvent(value: unknown, index: number): WorkEventRecordV1 {
  const path = `work_event_page.events[${index}]`;
  const object = exactObject(
    value,
    [
      "event_seq",
      "branch_id",
      "kind",
      "work_revision",
      "goal_revision",
      "criterion_set_revision",
      "branch_revision",
      "graph_revision",
      "source_ref",
      "created_at",
    ],
    path,
  );
  return {
    event_seq: positiveRevision(object.event_seq, `${path}.event_seq`),
    branch_id:
      object.branch_id === null
        ? null
        : resourceIdentity(object.branch_id, `${path}.branch_id`),
    kind: oneOf(object.kind, EVENT_KINDS, `${path}.kind`),
    work_revision: nullableRevision(object.work_revision, `${path}.work_revision`),
    goal_revision: nullableRevision(object.goal_revision, `${path}.goal_revision`),
    criterion_set_revision: nullableRevision(
      object.criterion_set_revision,
      `${path}.criterion_set_revision`,
    ),
    branch_revision: nullableRevision(
      object.branch_revision,
      `${path}.branch_revision`,
    ),
    graph_revision: nullableRevision(object.graph_revision, `${path}.graph_revision`),
    source_ref: opaqueIdentity(object.source_ref, `${path}.source_ref`),
    created_at: timestamp(object.created_at, `${path}.created_at`),
  };
}

/** Strict decoder for a bounded retained Work semantic timeline page. */
export function decodeWorkEventPageV1(value: unknown): WorkEventPageV1 {
  const path = "work_event_page";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "requested_after_event_seq",
      "next_after_event_seq",
      "event_head",
      "retained_from_event_seq",
      "seen_through_event_seq",
      "coverage",
      "has_more",
      "events",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  const requested = nullableRevision(
    object.requested_after_event_seq,
    `${path}.requested_after_event_seq`,
  );
  const next = nullableRevision(
    object.next_after_event_seq,
    `${path}.next_after_event_seq`,
  );
  const head = positiveRevision(object.event_head, `${path}.event_head`);
  const retained = positiveRevision(
    object.retained_from_event_seq,
    `${path}.retained_from_event_seq`,
  );
  const seen = nullableRevision(
    object.seen_through_event_seq,
    `${path}.seen_through_event_seq`,
  );
  if (!Array.isArray(object.events) || object.events.length > 100) {
    throw new TypeError(`${path}.events must be a bounded array`);
  }
  const events = object.events.map(decodeWorkEvent);
  const effectiveAfter = Math.max(requested ?? 0, retained - 1);
  events.forEach((event, index) => {
    if (event.event_seq !== effectiveAfter + index + 1) {
      throw new TypeError(`${path}.events contains a sequence gap`);
    }
  });
  const hasMore = object.has_more;
  if (typeof hasMore !== "boolean") {
    throw new TypeError(`${path}.has_more must be boolean`);
  }
  const expectedNext = events.at(-1)?.event_seq ?? requested;
  if (next !== expectedNext || (!hasMore && (next ?? effectiveAfter) !== head)) {
    throw new TypeError(`${path} cursor and event tail disagree`);
  }
  if (seen !== null && seen > head) {
    throw new TypeError(`${path}.seen_through_event_seq is ahead of the event head`);
  }
  const coverage = oneOf(
    object.coverage,
    ["complete", "expired"] as const,
    `${path}.coverage`,
  );
  if ((coverage === "expired") !== ((requested ?? 0) < retained - 1)) {
    throw new TypeError(`${path}.coverage disagrees with the retention floor`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    requested_after_event_seq: requested,
    next_after_event_seq: next,
    event_head: head,
    retained_from_event_seq: retained,
    seen_through_event_seq: seen,
    coverage,
    has_more: hasMore,
    events,
  };
}

function boundedText(value: unknown, maximum: number, path: string): string {
  const parsed = nonEmptyString(value, path);
  if (parsed.trim().length === 0 || new TextEncoder().encode(parsed).length > maximum) {
    throw new TypeError(`${path} must be non-empty and at most ${maximum} UTF-8 bytes`);
  }
  return parsed;
}

function decodeWorkCatalogCursor(
  value: unknown,
  path: string,
): WorkCatalogCursorV1 {
  const object = exactObject(value, ["created_at", "work_id"], path);
  return {
    created_at: timestamp(object.created_at, `${path}.created_at`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
  };
}

function decodeWorkCatalogEntry(value: unknown, index: number): WorkCatalogEntryV1 {
  const path = `work_catalog.entries[${index}]`;
  const object = exactObject(
    value,
    [
      "work_id",
      "goal",
      "work_revision",
      "delivery_branch_id",
      "delivery_branch_revision",
      "graph_revision",
      "graph_item_count",
      "pending_decision_count",
      "event_head",
      "seen_through_event_seq",
      "unseen_event_count",
      "attention",
      "delivery_branch_activity",
      "created_at",
      "last_activity_at",
    ],
    path,
  );
  const eventHead = positiveRevision(object.event_head, `${path}.event_head`);
  const seen = nullableRevision(
    object.seen_through_event_seq,
    `${path}.seen_through_event_seq`,
  );
  const unseen = safeIntegerAtLeast(
    object.unseen_event_count,
    0,
    `${path}.unseen_event_count`,
  );
  const pending = boundedCount(
    object.pending_decision_count,
    8,
    `${path}.pending_decision_count`,
  );
  const attention = oneOf(
    object.attention,
    ["needs_review", "updated", "none"] as const,
    `${path}.attention`,
  );
  const deliveryBranchActivity = oneOf(
    object.delivery_branch_activity,
    ["working", "waiting", "paused", "idle"] as const,
    `${path}.delivery_branch_activity`,
  );
  if (
    (seen ?? 0) > eventHead ||
    unseen !== eventHead - (seen ?? 0) ||
    (attention === "needs_review") !== (pending > 0) ||
    (attention === "updated") !== (pending === 0 && unseen > 0) ||
    (attention === "none") !== (pending === 0 && unseen === 0)
  ) {
    throw new TypeError(`${path} attention and cursor facts disagree`);
  }
  const createdAt = timestamp(object.created_at, `${path}.created_at`);
  const lastActivityAt = timestamp(
    object.last_activity_at,
    `${path}.last_activity_at`,
  );
  if (utcTimestampSortKey(lastActivityAt) < utcTimestampSortKey(createdAt)) {
    throw new TypeError(`${path}.last_activity_at precedes creation`);
  }
  return {
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    goal: boundedText(object.goal, 8 * 1024, `${path}.goal`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    delivery_branch_id: resourceIdentity(
      object.delivery_branch_id,
      `${path}.delivery_branch_id`,
    ),
    delivery_branch_revision: positiveRevision(
      object.delivery_branch_revision,
      `${path}.delivery_branch_revision`,
    ),
    graph_revision: positiveRevision(object.graph_revision, `${path}.graph_revision`),
    graph_item_count: boundedCount(
      object.graph_item_count,
      256,
      `${path}.graph_item_count`,
    ),
    pending_decision_count: pending,
    event_head: eventHead,
    seen_through_event_seq: seen,
    unseen_event_count: unseen,
    attention,
    delivery_branch_activity: deliveryBranchActivity,
    created_at: createdAt,
    last_activity_at: lastActivityAt,
  };
}

/** Strict decoder for one owner-scoped, keyset-paginated Work catalog page. */
export function decodeWorkCatalogPageV1(value: unknown): WorkCatalogPageV1 {
  const path = "work_catalog";
  const object = exactObject(value, ["schema_version", "entries", "next_cursor"], path);
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  if (!Array.isArray(object.entries) || object.entries.length > 50) {
    throw new TypeError(`${path}.entries must be a bounded array`);
  }
  const entries = object.entries.map(decodeWorkCatalogEntry);
  for (let index = 1; index < entries.length; index += 1) {
    const previous = entries[index - 1]!;
    const current = entries[index]!;
    const previousTime = utcTimestampSortKey(previous.created_at);
    const currentTime = utcTimestampSortKey(current.created_at);
    if (
      previousTime < currentTime ||
      (previousTime === currentTime && previous.work_id <= current.work_id)
    ) {
      throw new TypeError(`${path}.entries are not in canonical creation order`);
    }
  }
  const nextCursor =
    object.next_cursor === null
      ? null
      : decodeWorkCatalogCursor(object.next_cursor, `${path}.next_cursor`);
  if (nextCursor !== null) {
    const tail = entries.at(-1);
    if (
      !tail ||
      tail.created_at !== nextCursor.created_at ||
      tail.work_id !== nextCursor.work_id
    ) {
      throw new TypeError(`${path}.next_cursor does not identify the page tail`);
    }
  }
  return { schema_version: 1, entries, next_cursor: nextCursor };
}

function decodeWorkConversationHeadAt(
  value: unknown,
  path: string,
): WorkConversationHeadV1 {
  const head = exactObject(
    value,
    [
      "completed_turn",
      "journal_event_seq",
      "conversation_seq",
      "canonical_root_hash",
      "projection_schema",
      "compaction_generation",
      "config_version_id",
    ],
    path,
  );
  const root = nonEmptyString(head.canonical_root_hash, `${path}.canonical_root_hash`);
  if (!/^[0-9a-f]{64}$/u.test(root)) {
    throw new TypeError(`${path}.canonical_root_hash must be a canonical SHA-256 root`);
  }
  return {
    completed_turn: safeIntegerAtLeast(head.completed_turn, 1, `${path}.completed_turn`),
    journal_event_seq: safeIntegerAtLeast(
      head.journal_event_seq,
      1,
      `${path}.journal_event_seq`,
    ),
    conversation_seq: safeIntegerAtLeast(
      head.conversation_seq,
      1,
      `${path}.conversation_seq`,
    ),
    canonical_root_hash: root,
    projection_schema: safeIntegerAtLeast(
      head.projection_schema,
      1,
      `${path}.projection_schema`,
    ),
    compaction_generation: safeIntegerAtLeast(
      head.compaction_generation,
      0,
      `${path}.compaction_generation`,
    ),
    config_version_id:
      head.config_version_id === null
        ? null
        : opaqueIdentity(head.config_version_id, `${path}.config_version_id`),
  };
}

/** Strict public committed-conversation cursor used by Work branch operations. */
export function decodeWorkConversationHeadV1(value: unknown): WorkConversationHeadV1 {
  return decodeWorkConversationHeadAt(value, "work_conversation_head");
}

function sameWorkConversationHead(
  left: WorkConversationHeadV1 | null,
  right: WorkConversationHeadV1 | null,
): boolean {
  return (
    left === right ||
    (left !== null &&
      right !== null &&
      left.completed_turn === right.completed_turn &&
      left.journal_event_seq === right.journal_event_seq &&
      left.conversation_seq === right.conversation_seq &&
      left.canonical_root_hash === right.canonical_root_hash &&
      left.projection_schema === right.projection_schema &&
      left.compaction_generation === right.compaction_generation &&
      left.config_version_id === right.config_version_id)
  );
}

/** Strict public projection of a durable Work attachment and its current mode. */
export function decodeWorkBranchAttachmentV1(
  value: unknown,
): WorkBranchAttachmentV1 {
  const path = "work_attachment";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "branch_id",
      "attachment_id",
      "attachment_epoch",
      "branch_revision",
      "mode",
      "sync",
      "control_basis",
      "head",
      "attached_at",
      "expires_at",
    ],
    path,
  );
  if (
    object.schema_version !== 1 ||
    (object.mode !== "read_only" && object.mode !== "controller") ||
    object.sync !== "current"
  ) {
    throw new TypeError(`${path} has unsupported continuity semantics`);
  }
  const decodeHead = (value: unknown): WorkBranchAttachmentV1["head"] => {
    if (value === null) return null;
    return decodeWorkConversationHeadAt(value, `${path}.head`);
  };
  const attachedAt = timestamp(object.attached_at, `${path}.attached_at`);
  const expiresAt = timestamp(object.expires_at, `${path}.expires_at`);
  if (utcTimestampSortKey(expiresAt) <= utcTimestampSortKey(attachedAt)) {
    throw new TypeError(`${path}.expires_at must follow attachment creation`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    attachment_id: opaqueIdentity(object.attachment_id, `${path}.attachment_id`),
    attachment_epoch: positiveRevision(object.attachment_epoch, `${path}.attachment_epoch`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    mode: object.mode,
    sync: "current",
    control_basis: decodeWorkBranchControlBasisV1(
      object.control_basis,
      `${path}.control_basis`,
    ),
    head: decodeHead(object.head),
    attached_at: attachedAt,
    expires_at: expiresAt,
  };
}

function decodeWorkBranchControlBasisV1(
  value: unknown,
  path: string,
): WorkBranchControlBasisV1 {
  const object = exactObject(value, ["writer_epoch", "canonical_root_hash"], path);
  const root =
    object.canonical_root_hash === null
      ? null
      : nonEmptyString(object.canonical_root_hash, `${path}.canonical_root_hash`);
  if (root !== null && !/^[0-9a-f]{64}$/u.test(root)) {
    throw new TypeError(`${path}.canonical_root_hash must be a canonical SHA-256 hash`);
  }
  return {
    writer_epoch: safeIntegerAtLeast(object.writer_epoch, 0, `${path}.writer_epoch`),
    canonical_root_hash: root,
  };
}

/** Strict projection of one durable branch-control command. */
export function decodeWorkBranchControlOperationV2(
  value: unknown,
): WorkBranchControlOperationV2 {
  const path = "work_control_operation";
  const hasProgress =
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.prototype.hasOwnProperty.call(value, "progress");
  const object = exactObject(
    value,
    [
      "schema_version",
      "operation_id",
      "work_id",
      "branch_id",
      "attachment_id",
      "kind",
      "state",
      "outcome",
      "branch_revision",
      "control_basis",
      ...(hasProgress ? ["progress"] : []),
      "created_at",
      "completed_at",
    ],
    path,
  );
  if (object.schema_version !== 2) {
    throw new TypeError(`${path}.schema_version must be 2`);
  }
  if (
    object.kind !== "acquire_branch_control" &&
    object.kind !== "force_takeover" &&
    object.kind !== "release_branch_control"
  ) {
    throw new TypeError(`${path}.kind is unsupported`);
  }
  if (
    object.state !== "pending" &&
    object.state !== "aborted" &&
    object.state !== "succeeded" &&
    object.state !== "conflict"
  ) {
    throw new TypeError(`${path}.state is unsupported`);
  }
  const outcomes = [
    "pending",
    "aborted",
    "acquired",
    "already_controlled",
    "taken_over",
    "released",
    "already_released",
    "writer_conflict",
    "branch_revision_conflict",
    "head_conflict",
  ] as const;
  if (!outcomes.some((outcome) => outcome === object.outcome)) {
    throw new TypeError(`${path}.outcome is unsupported`);
  }
  const pending = object.state === "pending";
  if (pending !== (object.outcome === "pending")) {
    throw new TypeError(`${path} pending state and outcome disagree`);
  }
  const aborted = object.state === "aborted";
  if (aborted !== (object.outcome === "aborted")) {
    throw new TypeError(`${path} aborted state and outcome disagree`);
  }
  const success = object.state === "succeeded";
  const successfulOutcome =
    object.outcome === "acquired" ||
    object.outcome === "already_controlled" ||
    object.outcome === "taken_over" ||
    object.outcome === "released" ||
    object.outcome === "already_released";
  if (!pending && !aborted && success !== successfulOutcome) {
    throw new TypeError(`${path} state and outcome disagree`);
  }
  const createdAt = timestamp(object.created_at, `${path}.created_at`);
  const completedAt =
    object.completed_at === null
      ? null
      : timestamp(object.completed_at, `${path}.completed_at`);
  if (pending !== (completedAt === null)) {
    throw new TypeError(`${path} state and completion time disagree`);
  }
  let progress: WorkBranchControlOperationV2["progress"];
  if (pending) {
    const progressObject = exactObject(object.progress, ["phase", "abortable"], `${path}.progress`);
    const phases = [
      "awaiting_reauthentication",
      "preparing",
      "fencing",
      "sealing_effects",
      "activating",
    ] as const;
    if (!phases.some((phase) => phase === progressObject.phase)) {
      throw new TypeError(`${path}.progress.phase is unsupported`);
    }
    if (typeof progressObject.abortable !== "boolean") {
      throw new TypeError(`${path}.progress.abortable must be boolean`);
    }
    progress = {
      phase: progressObject.phase as NonNullable<WorkBranchControlOperationV2["progress"]>["phase"],
      abortable: progressObject.abortable,
    };
  } else if (object.progress !== undefined) {
    throw new TypeError(`${path}.progress is only valid while pending`);
  }
  if (
    completedAt !== null &&
    utcTimestampSortKey(completedAt) < utcTimestampSortKey(createdAt)
  ) {
    throw new TypeError(`${path}.completed_at precedes creation`);
  }
  return {
    schema_version: 2,
    operation_id: resourceIdentity(object.operation_id, `${path}.operation_id`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    attachment_id: opaqueIdentity(object.attachment_id, `${path}.attachment_id`),
    kind: object.kind,
    state: object.state,
    outcome: object.outcome as WorkBranchControlOperationV2["outcome"],
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    control_basis:
      object.control_basis === null
        ? null
        : decodeWorkBranchControlBasisV1(object.control_basis, `${path}.control_basis`),
    ...(progress === undefined ? {} : { progress }),
    created_at: createdAt,
    completed_at: completedAt,
  };
}

/** Strict projection of one durable alternative-branch creation. */
export function decodeWorkBranchCreationOperationV1(
  value: unknown,
): WorkBranchCreationOperationV1 {
  const path = "work_branch_creation_operation";
  const object = exactObject(
    value,
    [
      "schema_version",
      "operation_id",
      "work_id",
      "origin_branch_id",
      "child_branch_id",
      "fork_cursor",
      "state",
      "outcome",
      "origin_branch_revision",
      "created_at",
      "completed_at",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version must be 1`);
  }
  const state = oneOf(
    object.state,
    ["pending", "aborted", "succeeded", "conflict"] as const,
    `${path}.state`,
  );
  const outcome = oneOf(
    object.outcome,
    [
      "pending",
      "aborted",
      "created",
      "branch_revision_conflict",
      "cursor_conflict",
      "capacity_exceeded",
    ] as const,
    `${path}.outcome`,
  );
  const consistent =
    (state === "pending" && outcome === "pending") ||
    (state === "aborted" && outcome === "aborted") ||
    (state === "succeeded" && outcome === "created") ||
    (state === "conflict" &&
      (outcome === "branch_revision_conflict" ||
        outcome === "cursor_conflict" ||
        outcome === "capacity_exceeded"));
  if (!consistent) {
    throw new TypeError(`${path} state and outcome disagree`);
  }
  const createdAt = timestamp(object.created_at, `${path}.created_at`);
  const completedAt = nullableTimestamp(object.completed_at, `${path}.completed_at`);
  if ((state === "pending") !== (completedAt === null)) {
    throw new TypeError(`${path} state and completion time disagree`);
  }
  if (
    completedAt !== null &&
    utcTimestampSortKey(completedAt) < utcTimestampSortKey(createdAt)
  ) {
    throw new TypeError(`${path}.completed_at precedes creation`);
  }
  return {
    schema_version: 1,
    operation_id: resourceIdentity(object.operation_id, `${path}.operation_id`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    origin_branch_id: resourceIdentity(
      object.origin_branch_id,
      `${path}.origin_branch_id`,
    ),
    child_branch_id: resourceIdentity(object.child_branch_id, `${path}.child_branch_id`),
    fork_cursor: contentHash(object.fork_cursor, `${path}.fork_cursor`),
    state,
    outcome,
    origin_branch_revision: positiveRevision(
      object.origin_branch_revision,
      `${path}.origin_branch_revision`,
    ),
    created_at: createdAt,
    completed_at: completedAt,
  };
}

/** Strict projection of one durable, recoverable branch deletion. */
export function decodeWorkBranchDeletionOperationV1(
  value: unknown,
): WorkBranchDeletionOperationV1 {
  const path = "work_branch_deletion_operation";
  const object = exactObject(
    value,
    [
      "schema_version",
      "operation_id",
      "work_id",
      "branch_id",
      "state",
      "phase",
      "outcome",
      "work_revision",
      "branch_revision",
      "created_at",
      "completed_at",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version must be 1`);
  }
  const state = oneOf(
    object.state,
    ["pending", "succeeded", "conflict"] as const,
    `${path}.state`,
  );
  const phase = oneOf(
    object.phase,
    ["fence", "session_cleanup", "lineage_gc", "branch_cleanup", "complete"] as const,
    `${path}.phase`,
  );
  const outcome = oneOf(
    object.outcome,
    [
      "pending",
      "deleted",
      "delivery_branch_protected",
      "work_revision_conflict",
      "branch_revision_conflict",
    ] as const,
    `${path}.outcome`,
  );
  const terminalConflict =
    outcome === "delivery_branch_protected" ||
    outcome === "work_revision_conflict" ||
    outcome === "branch_revision_conflict";
  const consistent =
    (state === "pending" && phase !== "complete" && outcome === "pending") ||
    (state === "succeeded" && phase === "complete" && outcome === "deleted") ||
    (state === "conflict" && phase === "complete" && terminalConflict);
  if (!consistent) {
    throw new TypeError(`${path} state, phase, and outcome disagree`);
  }
  const createdAt = timestamp(object.created_at, `${path}.created_at`);
  const completedAt = nullableTimestamp(object.completed_at, `${path}.completed_at`);
  if ((state === "pending") !== (completedAt === null)) {
    throw new TypeError(`${path} state and completion time disagree`);
  }
  if (
    completedAt !== null &&
    utcTimestampSortKey(completedAt) < utcTimestampSortKey(createdAt)
  ) {
    throw new TypeError(`${path}.completed_at precedes creation`);
  }
  return {
    schema_version: 1,
    operation_id: resourceIdentity(object.operation_id, `${path}.operation_id`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    state,
    phase,
    outcome,
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    created_at: createdAt,
    completed_at: completedAt,
  };
}

function decodeWorkBranchCatalogEntryV1(
  value: unknown,
  index: number,
): WorkBranchCatalogEntryV1 {
  const path = `work_branch_catalog.branches[${index}]`;
  const object = exactObject(
    value,
    [
      "branch_id",
      "branch_revision",
      "is_delivery",
      "origin_branch_id",
      "fork_cursor",
      "goal_revision_ref",
      "criteria_set_revision_ref",
      "basis_graph_revision",
      "current_graph_revision",
      "materialization",
      "created_at",
    ],
    path,
  );
  if (typeof object.is_delivery !== "boolean") {
    throw new TypeError(`${path}.is_delivery must be boolean`);
  }
  const originBranchId =
    object.origin_branch_id === null
      ? null
      : resourceIdentity(object.origin_branch_id, `${path}.origin_branch_id`);
  const forkCursor =
    object.fork_cursor === null
      ? null
      : contentHash(object.fork_cursor, `${path}.fork_cursor`);
  if ((originBranchId === null) !== (forkCursor === null)) {
    throw new TypeError(`${path} has incomplete fork lineage`);
  }
  const basisGraphRevision = positiveRevision(
    object.basis_graph_revision,
    `${path}.basis_graph_revision`,
  );
  const currentGraphRevision = positiveRevision(
    object.current_graph_revision,
    `${path}.current_graph_revision`,
  );
  if (currentGraphRevision < basisGraphRevision) {
    throw new TypeError(`${path}.current_graph_revision precedes its fork basis`);
  }
  const expectedDimensions = [
    "conversation",
    "goal",
    "criteria",
    "task_graph",
    "checkpoint",
    "workspace",
    "artifacts",
    "transient_authority",
  ] as const;
  let materialization: WorkBranchCatalogEntryV1["materialization"] = null;
  if (object.materialization !== null) {
    if (!Array.isArray(object.materialization) || object.materialization.length !== 8) {
      throw new TypeError(`${path}.materialization must contain every fork dimension`);
    }
    materialization = object.materialization.map((entry, dimensionIndex) => {
      const entryPath = `${path}.materialization[${dimensionIndex}]`;
      const summary = exactObject(entry, ["dimension", "disposition"], entryPath);
      const dimension = oneOf(summary.dimension, expectedDimensions, `${entryPath}.dimension`);
      if (dimension !== expectedDimensions[dimensionIndex]) {
        throw new TypeError(`${path}.materialization is not in canonical dimension order`);
      }
      return {
        dimension,
        disposition: oneOf(
          summary.disposition,
          ["shared", "copied", "rebased", "excluded", "gap"] as const,
          `${entryPath}.disposition`,
        ),
      };
    });
  }
  if ((originBranchId === null) !== (materialization === null)) {
    throw new TypeError(`${path} has contradictory fork materialization`);
  }
  return {
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    is_delivery: object.is_delivery,
    origin_branch_id: originBranchId,
    fork_cursor: forkCursor,
    goal_revision_ref: positiveRevision(
      object.goal_revision_ref,
      `${path}.goal_revision_ref`,
    ),
    criteria_set_revision_ref: positiveRevision(
      object.criteria_set_revision_ref,
      `${path}.criteria_set_revision_ref`,
    ),
    basis_graph_revision: basisGraphRevision,
    current_graph_revision: currentGraphRevision,
    materialization,
    created_at: timestamp(object.created_at, `${path}.created_at`),
  };
}

/** Strict complete active-branch catalog; active admission caps it at 32. */
export function decodeWorkBranchCatalogV1(value: unknown): WorkBranchCatalogV1 {
  const path = "work_branch_catalog";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "work_revision", "delivery_branch_id", "branches"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version must be 1`);
  }
  if (!Array.isArray(object.branches) || object.branches.length < 1 || object.branches.length > 32) {
    throw new TypeError(`${path}.branches must contain between 1 and 32 active branches`);
  }
  const branches = object.branches.map(decodeWorkBranchCatalogEntryV1);
  for (let index = 1; index < branches.length; index += 1) {
    const previous = branches[index - 1]!;
    const current = branches[index]!;
    const previousTime = utcTimestampSortKey(previous.created_at);
    const currentTime = utcTimestampSortKey(current.created_at);
    if (
      previousTime > currentTime ||
      (previousTime === currentTime && previous.branch_id >= current.branch_id)
    ) {
      throw new TypeError(`${path}.branches are not in canonical creation order`);
    }
  }
  const deliveryBranchId = resourceIdentity(
    object.delivery_branch_id,
    `${path}.delivery_branch_id`,
  );
  if (
    branches.filter((branch) => branch.is_delivery).length !== 1 ||
    !branches.some(
      (branch) => branch.is_delivery && branch.branch_id === deliveryBranchId,
    )
  ) {
    throw new TypeError(`${path} has contradictory delivery branch identity`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    delivery_branch_id: deliveryBranchId,
    branches,
  };
}

/** Strict archive-time cursor page; archived history is never returned unbounded. */
export function decodeWorkArchivedBranchPageV1(
  value: unknown,
): WorkArchivedBranchPageV1 {
  const path = "work_archived_branches";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "work_revision", "branches", "next_cursor"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version must be 1`);
  }
  if (!Array.isArray(object.branches) || object.branches.length > 100) {
    throw new TypeError(`${path}.branches exceeds the bounded page size`);
  }
  const branches = object.branches.map((value, index) => {
    const entryPath = `${path}.branches[${index}]`;
    const entry = exactObject(
      value,
      [
        "branch_id",
        "branch_revision",
        "origin_branch_id",
        "archived_at",
        "created_at",
      ],
      entryPath,
    );
    return {
      branch_id: resourceIdentity(entry.branch_id, `${entryPath}.branch_id`),
      branch_revision: positiveRevision(
        entry.branch_revision,
        `${entryPath}.branch_revision`,
      ),
      origin_branch_id:
        entry.origin_branch_id === null
          ? null
          : resourceIdentity(entry.origin_branch_id, `${entryPath}.origin_branch_id`),
      archived_at: timestamp(entry.archived_at, `${entryPath}.archived_at`),
      created_at: timestamp(entry.created_at, `${entryPath}.created_at`),
    };
  });
  for (let index = 1; index < branches.length; index += 1) {
    const previous = branches[index - 1]!;
    const current = branches[index]!;
    const previousTime = utcTimestampSortKey(previous.archived_at);
    const currentTime = utcTimestampSortKey(current.archived_at);
    if (
      previousTime < currentTime ||
      (previousTime === currentTime && previous.branch_id <= current.branch_id)
    ) {
      throw new TypeError(`${path}.branches are not in canonical archive order`);
    }
  }
  let nextCursor: WorkArchivedBranchPageV1["next_cursor"] = null;
  if (object.next_cursor !== null) {
    const cursor = exactObject(
      object.next_cursor,
      ["archived_at", "branch_id"],
      `${path}.next_cursor`,
    );
    nextCursor = {
      archived_at: timestamp(cursor.archived_at, `${path}.next_cursor.archived_at`),
      branch_id: resourceIdentity(cursor.branch_id, `${path}.next_cursor.branch_id`),
    };
    const last = branches.at(-1);
    if (
      !last ||
      last.archived_at !== nextCursor.archived_at ||
      last.branch_id !== nextCursor.branch_id
    ) {
      throw new TypeError(`${path}.next_cursor does not seal the last branch`);
    }
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    branches,
    next_cursor: nextCursor,
  };
}

function decodeWorkBranchComparisonSide(
  value: unknown,
  path: string,
): WorkBranchComparisonReportV2["left"] {
  const object = exactObject(
    value,
    ["branch_id", "branch_revision", "is_delivery", "goal_revision_ref", "criteria", "graph", "subject"],
    path,
  );
  if (typeof object.is_delivery !== "boolean") {
    throw new TypeError(`${path}.is_delivery must be boolean`);
  }
  const criteria = exactObject(
    object.criteria,
    ["revision", "manifest_hash", "member_count"],
    `${path}.criteria`,
  );
  const graph = exactObject(
    object.graph,
    ["basis_revision", "current_revision", "manifest_hash", "item_count", "edge_count"],
    `${path}.graph`,
  );
  const basisRevision = positiveRevision(graph.basis_revision, `${path}.graph.basis_revision`);
  const currentRevision = positiveRevision(
    graph.current_revision,
    `${path}.graph.current_revision`,
  );
  if (currentRevision < basisRevision) {
    throw new TypeError(`${path}.graph.current_revision precedes its basis`);
  }
  let subject: WorkBranchComparisonReportV2["left"]["subject"] = null;
  if (object.subject !== null) {
    const source = exactObject(
      object.subject,
      ["subject_ref", "subject_revision", "graph_revision"],
      `${path}.subject`,
    );
    const subjectRef = opaqueIdentity(source.subject_ref, `${path}.subject.subject_ref`);
    if (subjectRef.length > 256) {
      throw new TypeError(`${path}.subject.subject_ref exceeds its bound`);
    }
    subject = {
      subject_ref: subjectRef,
      subject_revision: contentHash(
        source.subject_revision,
        `${path}.subject.subject_revision`,
      ),
      graph_revision: positiveRevision(
        source.graph_revision,
        `${path}.subject.graph_revision`,
      ),
    };
  }
  return {
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    is_delivery: object.is_delivery,
    goal_revision_ref: positiveRevision(object.goal_revision_ref, `${path}.goal_revision_ref`),
    criteria: {
      revision: positiveRevision(criteria.revision, `${path}.criteria.revision`),
      manifest_hash: contentHash(criteria.manifest_hash, `${path}.criteria.manifest_hash`),
      member_count: boundedCount(criteria.member_count, 128, `${path}.criteria.member_count`),
    },
    graph: {
      basis_revision: basisRevision,
      current_revision: currentRevision,
      manifest_hash: contentHash(graph.manifest_hash, `${path}.graph.manifest_hash`),
      item_count: boundedCount(graph.item_count, 256, `${path}.graph.item_count`),
      edge_count: boundedCount(graph.edge_count, 1024, `${path}.graph.edge_count`),
    },
    subject,
  };
}

/** Strict deterministic comparison facts; uncovered domains stay explicit gaps. */
export function decodeWorkBranchComparisonReportV2(
  value: unknown,
): WorkBranchComparisonReportV2 {
  const path = "work_branch_comparison";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "work_revision",
      "directly_comparable",
      "blockers",
      "graph_relation",
      "subject_relation",
      "evidence_relation",
      "left",
      "right",
      "left_evidence",
      "right_evidence",
      "coverage_gaps",
    ],
    path,
  );
  if (object.schema_version !== 2 || typeof object.directly_comparable !== "boolean") {
    throw new TypeError(`${path} has invalid schema or comparability`);
  }
  const canonicalStrings = <T extends string>(
    value: unknown,
    allowed: readonly T[],
    field: string,
  ): T[] => {
    if (!Array.isArray(value)) throw new TypeError(`${field} must be an array`);
    const decoded = value.map((entry, index) => oneOf(entry, allowed, `${field}[${index}]`));
    const positions = decoded.map((entry) => allowed.indexOf(entry));
    if (new Set(decoded).size !== decoded.length || positions.some((position, index) => index > 0 && position <= positions[index - 1]!)) {
      throw new TypeError(`${field} must be unique and canonically ordered`);
    }
    return decoded;
  };
  const blockers = canonicalStrings(
    object.blockers,
    ["goal_revision_differs", "criteria_revision_differs"] as const,
    `${path}.blockers`,
  );
  if (object.directly_comparable !== (blockers.length === 0)) {
    throw new TypeError(`${path}.directly_comparable contradicts blockers`);
  }
  const left = decodeWorkBranchComparisonSide(object.left, `${path}.left`);
  const right = decodeWorkBranchComparisonSide(object.right, `${path}.right`);
  if (left.branch_id === right.branch_id || Number(left.is_delivery) + Number(right.is_delivery) > 1) {
    throw new TypeError(`${path} has contradictory branch identities`);
  }
  const expectedBlockers = [
    ...(left.goal_revision_ref === right.goal_revision_ref
      ? []
      : (["goal_revision_differs"] as const)),
    ...(left.criteria.revision === right.criteria.revision &&
    left.criteria.manifest_hash === right.criteria.manifest_hash
      ? []
      : (["criteria_revision_differs"] as const)),
  ];
  if (
    blockers.length !== expectedBlockers.length ||
    blockers.some((blocker, index) => blocker !== expectedBlockers[index])
  ) {
    throw new TypeError(`${path}.blockers contradict branch bases`);
  }
  if (
    left.criteria.manifest_hash === right.criteria.manifest_hash &&
    left.criteria.member_count !== right.criteria.member_count
  ) {
    throw new TypeError(`${path} has contradictory criterion manifest counts`);
  }
  const graphRelation = oneOf(
    object.graph_relation,
    ["same", "different", "unavailable"] as const,
    `${path}.graph_relation`,
  );
  const expectedGraphRelation =
    left.graph.manifest_hash === right.graph.manifest_hash ? "same" : "different";
  if (
    expectedGraphRelation === "same" &&
    (left.graph.item_count !== right.graph.item_count ||
      left.graph.edge_count !== right.graph.edge_count)
  ) {
    throw new TypeError(`${path} has contradictory graph manifest counts`);
  }
  if (graphRelation !== expectedGraphRelation) {
    throw new TypeError(`${path}.graph_relation contradicts graph facts`);
  }
  const subjectRelation = oneOf(
    object.subject_relation,
    ["same", "different", "unavailable"] as const,
    `${path}.subject_relation`,
  );
  const subjectsCurrent =
    left.subject !== null &&
    right.subject !== null &&
    left.subject.graph_revision === left.graph.current_revision &&
    right.subject.graph_revision === right.graph.current_revision;
  const expectedSubjectRelation = !subjectsCurrent
    ? "unavailable"
    : left.subject!.subject_ref === right.subject!.subject_ref &&
        left.subject!.subject_revision === right.subject!.subject_revision
      ? "same"
      : "different";
  if (subjectRelation !== expectedSubjectRelation) {
    throw new TypeError(`${path}.subject_relation contradicts subject facts`);
  }
  const decodeEvidence = (value: unknown, field: string, requiredCount: number) => {
    const evidence = exactObject(
      value,
      ["manifest_hash", "required_count", "fresh_check_count", "accepted_gap_count"],
      field,
    );
    const decoded = {
      manifest_hash: contentHash(evidence.manifest_hash, `${field}.manifest_hash`),
      required_count: boundedCount(evidence.required_count, 128, `${field}.required_count`),
      fresh_check_count: boundedCount(
        evidence.fresh_check_count,
        128,
        `${field}.fresh_check_count`,
      ),
      accepted_gap_count: boundedCount(
        evidence.accepted_gap_count,
        128,
        `${field}.accepted_gap_count`,
      ),
    };
    if (
      decoded.required_count !== requiredCount ||
      decoded.fresh_check_count > decoded.required_count ||
      decoded.accepted_gap_count > decoded.required_count
    ) {
      throw new TypeError(`${field} contradicts its criterion-set bounds`);
    }
    return decoded;
  };
  const leftEvidence = decodeEvidence(
    object.left_evidence,
    `${path}.left_evidence`,
    left.criteria.member_count,
  );
  const rightEvidence = decodeEvidence(
    object.right_evidence,
    `${path}.right_evidence`,
    right.criteria.member_count,
  );
  const evidenceRelation = oneOf(
    object.evidence_relation,
    ["same", "different", "unavailable"] as const,
    `${path}.evidence_relation`,
  );
  const expectedEvidenceRelation =
    leftEvidence.manifest_hash === rightEvidence.manifest_hash ? "same" : "different";
  if (
    evidenceRelation !== expectedEvidenceRelation ||
    (expectedEvidenceRelation === "same" &&
      (leftEvidence.required_count !== rightEvidence.required_count ||
        leftEvidence.fresh_check_count !== rightEvidence.fresh_check_count ||
        leftEvidence.accepted_gap_count !== rightEvidence.accepted_gap_count))
  ) {
    throw new TypeError(`${path}.evidence_relation contradicts evidence facts`);
  }
  return {
    schema_version: 2,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    directly_comparable: object.directly_comparable,
    blockers,
    graph_relation: graphRelation,
    subject_relation: subjectRelation,
    evidence_relation: evidenceRelation,
    left,
    right,
    left_evidence: leftEvidence,
    right_evidence: rightEvidence,
    coverage_gaps: canonicalStrings(
      object.coverage_gaps,
      ["change_details", "risks", "time_cost"] as const,
      `${path}.coverage_gaps`,
    ),
  };
}

/** Immutable patch provenance. Patch bytes are fetched explicitly elsewhere;
 * internal session identity is never part of this Work projection. */
export function decodeWorkPatchArtifactV1(value: unknown): WorkPatchArtifactV1 {
  const path = "work_patch_artifact";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "branch_id",
      "patch_artifact_id",
      "source_branch_revision",
      "source_graph_revision",
      "base_subject_revision",
      "result_subject_revision",
      "payload_hash",
      "payload_bytes",
      "format",
      "provider_invocation_ref",
      "source_ref",
      "created_at",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    patch_artifact_id: resourceIdentity(
      object.patch_artifact_id,
      `${path}.patch_artifact_id`,
    ),
    source_branch_revision: positiveRevision(
      object.source_branch_revision,
      `${path}.source_branch_revision`,
    ),
    source_graph_revision: positiveRevision(
      object.source_graph_revision,
      `${path}.source_graph_revision`,
    ),
    base_subject_revision: contentHash(
      object.base_subject_revision,
      `${path}.base_subject_revision`,
    ),
    result_subject_revision: contentHash(
      object.result_subject_revision,
      `${path}.result_subject_revision`,
    ),
    payload_hash: contentHash(object.payload_hash, `${path}.payload_hash`),
    payload_bytes: boundedCount(
      object.payload_bytes,
      16 * 1024 * 1024,
      `${path}.payload_bytes`,
    ),
    format: oneOf(object.format, ["unified_diff_v1"] as const, `${path}.format`),
    provider_invocation_ref: opaqueIdentity(
      object.provider_invocation_ref,
      `${path}.provider_invocation_ref`,
    ),
    source_ref: opaqueIdentity(object.source_ref, `${path}.source_ref`),
    created_at: timestamp(object.created_at, `${path}.created_at`),
  };
}

export function decodeWorkPatchArtifactPageV1(value: unknown): WorkPatchArtifactPageV1 {
  const path = "work_patch_artifact_page";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "branch_id", "artifacts", "next_cursor"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  if (!Array.isArray(object.artifacts) || object.artifacts.length > 50) {
    throw new TypeError(`${path}.artifacts must be a bounded array`);
  }
  const workId = resourceIdentity(object.work_id, `${path}.work_id`);
  const branchId = resourceIdentity(object.branch_id, `${path}.branch_id`);
  const artifacts = object.artifacts.map((entry, index) => {
    const artifact = decodeWorkPatchArtifactV1(entry);
    if (artifact.work_id !== workId || artifact.branch_id !== branchId) {
      throw new TypeError(`${path}.artifacts[${index}] belongs to another Work branch`);
    }
    return artifact;
  });
  for (let index = 1; index < artifacts.length; index += 1) {
    const previous = artifacts[index - 1]!;
    const current = artifacts[index]!;
    const previousTime = utcTimestampSortKey(previous.created_at);
    const currentTime = utcTimestampSortKey(current.created_at);
    if (
      previousTime < currentTime ||
      (previousTime === currentTime &&
        previous.patch_artifact_id <= current.patch_artifact_id)
    ) {
      throw new TypeError(`${path}.artifacts is not in canonical descending order`);
    }
  }
  let nextCursor: WorkPatchArtifactCursorV1 | null = null;
  if (object.next_cursor !== null) {
    const cursor = exactObject(
      object.next_cursor,
      ["created_at", "patch_artifact_id"],
      `${path}.next_cursor`,
    );
    const createdAt = timestamp(cursor.created_at, `${path}.next_cursor.created_at`);
    const patchArtifactId = resourceIdentity(
      cursor.patch_artifact_id,
      `${path}.next_cursor.patch_artifact_id`,
    );
    const last = artifacts.at(-1);
    if (
      last == null ||
      last.created_at !== createdAt ||
      last.patch_artifact_id !== patchArtifactId
    ) {
      throw new TypeError(`${path}.next_cursor must identify the last returned artifact`);
    }
    nextCursor = { created_at: createdAt, patch_artifact_id: patchArtifactId };
  }
  return {
    schema_version: 1,
    work_id: workId,
    branch_id: branchId,
    artifacts,
    next_cursor: nextCursor,
  };
}

/** Durable exact-base patch application. Executor leases remain internal. */
export function decodeWorkPatchMaterializationOperationV2(
  value: unknown,
): WorkPatchMaterializationOperationV2 {
  const path = "work_patch_materialization";
  const object = exactObject(
    value,
    [
      "schema_version",
      "operation_id",
      "work_id",
      "request_id",
      "patch_artifact_id",
      "source_branch_id",
      "target_branch_id",
      "target_branch_revision",
      "target_graph_revision",
      "base_subject_revision",
      "result_subject_revision",
      "payload_hash",
      "provider_ref",
      "policy_decision_ref",
      "state",
      "phase",
      "apply_invocation_ref",
      "observed_subject_revision",
      "apply_outcome",
      "failure_code",
      "verification_evidence_hash",
      "verification_outcome",
      "created_at",
      "completed_at",
    ],
    path,
  );
  if (object.schema_version !== 2) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  const state = oneOf(
    object.state,
    ["pending", "aborted", "succeeded", "conflict", "failed"] as const,
    `${path}.state`,
  );
  const phase = oneOf(
    object.phase,
    ["awaiting_dispatch", "applying", "reconciling", "verifying", "complete"] as const,
    `${path}.phase`,
  );
  const applyInvocationRef =
    object.apply_invocation_ref === null
      ? null
      : opaqueIdentity(object.apply_invocation_ref, `${path}.apply_invocation_ref`);
  const observedSubjectRevision =
    object.observed_subject_revision === null
      ? null
      : contentHash(
          object.observed_subject_revision,
          `${path}.observed_subject_revision`,
        );
  const applyOutcome =
    object.apply_outcome === null
      ? null
      : oneOf(
          object.apply_outcome,
          ["applied", "not_applied", "result_mismatch", "target_changed"] as const,
          `${path}.apply_outcome`,
        );
  const failureCode =
    object.failure_code === null
      ? null
      : oneOf(
          object.failure_code,
          [
            "provider_unavailable",
            "authorization_denied",
            "workspace_unavailable",
            "patch_rejected",
            "invocation_cancelled",
            "provider_internal",
          ] as const,
          `${path}.failure_code`,
        );
  const verificationEvidenceHash =
    object.verification_evidence_hash === null
      ? null
      : contentHash(
          object.verification_evidence_hash,
          `${path}.verification_evidence_hash`,
        );
  const verificationOutcome =
    object.verification_outcome === null
      ? null
      : oneOf(
          object.verification_outcome,
          ["passed", "target_changed"] as const,
          `${path}.verification_outcome`,
        );
  const completedAt = nullableTimestamp(object.completed_at, `${path}.completed_at`);
  if (
    (state === "pending" && (phase === "complete" || completedAt !== null)) ||
    (state !== "pending" && (phase !== "complete" || completedAt === null))
  ) {
    throw new TypeError(`${path} state, phase, and completion disagree`);
  }
  if (
    (phase === "awaiting_dispatch" &&
      (applyInvocationRef !== null ||
        observedSubjectRevision !== null ||
        applyOutcome !== null ||
        failureCode !== null)) ||
    ((phase === "applying" || phase === "reconciling") &&
      (applyInvocationRef === null ||
        observedSubjectRevision !== null ||
        applyOutcome !== null ||
        failureCode !== null)) ||
    (phase === "verifying" &&
      (applyInvocationRef === null ||
        observedSubjectRevision === null ||
        applyOutcome !== "applied" ||
        failureCode !== null)) ||
    (applyOutcome === "not_applied" &&
      (state !== "failed" || observedSubjectRevision !== null || failureCode === null)) ||
    ((applyOutcome === "applied" ||
      applyOutcome === "result_mismatch" ||
      applyOutcome === "target_changed") &&
      (applyInvocationRef === null ||
        observedSubjectRevision === null ||
        failureCode !== null)) ||
    (applyOutcome === null && state !== "pending" && state !== "aborted") ||
    (state === "aborted" &&
      (applyInvocationRef !== null ||
        observedSubjectRevision !== null ||
        applyOutcome !== null ||
        failureCode !== null)) ||
    ((applyOutcome === "result_mismatch" || applyOutcome === "target_changed") &&
      (state !== "conflict" || phase !== "complete"))
  ) {
    throw new TypeError(`${path} apply outcome contradicts lifecycle state`);
  }
  if (
    (verificationOutcome === null && verificationEvidenceHash !== null) ||
    (verificationOutcome === "passed" &&
      (verificationEvidenceHash === null || state !== "succeeded" || phase !== "complete")) ||
    (verificationOutcome === "target_changed" &&
      (verificationEvidenceHash !== null || state !== "conflict" || phase !== "complete"))
  ) {
    throw new TypeError(`${path} verification outcome contradicts evidence or lifecycle`);
  }
  if (
    (state === "succeeded" &&
      (applyOutcome !== "applied" || verificationOutcome !== "passed")) ||
    (phase === "verifying" && verificationOutcome !== null)
  ) {
    throw new TypeError(`${path} terminal success requires exact applied and verified facts`);
  }
  const baseSubjectRevision = contentHash(
    object.base_subject_revision,
    `${path}.base_subject_revision`,
  );
  const resultSubjectRevision = contentHash(
    object.result_subject_revision,
    `${path}.result_subject_revision`,
  );
  if (baseSubjectRevision === resultSubjectRevision) {
    throw new TypeError(`${path} patch result must change the subject revision`);
  }
  return {
    schema_version: 2,
    operation_id: resourceIdentity(object.operation_id, `${path}.operation_id`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    request_id: opaqueIdentity(object.request_id, `${path}.request_id`),
    patch_artifact_id: resourceIdentity(
      object.patch_artifact_id,
      `${path}.patch_artifact_id`,
    ),
    source_branch_id: resourceIdentity(object.source_branch_id, `${path}.source_branch_id`),
    target_branch_id: resourceIdentity(object.target_branch_id, `${path}.target_branch_id`),
    target_branch_revision: positiveRevision(
      object.target_branch_revision,
      `${path}.target_branch_revision`,
    ),
    target_graph_revision: positiveRevision(
      object.target_graph_revision,
      `${path}.target_graph_revision`,
    ),
    base_subject_revision: baseSubjectRevision,
    result_subject_revision: resultSubjectRevision,
    payload_hash: contentHash(object.payload_hash, `${path}.payload_hash`),
    provider_ref: opaqueIdentity(object.provider_ref, `${path}.provider_ref`),
    policy_decision_ref: opaqueIdentity(
      object.policy_decision_ref,
      `${path}.policy_decision_ref`,
    ),
    state,
    phase,
    apply_invocation_ref: applyInvocationRef,
    observed_subject_revision: observedSubjectRevision,
    apply_outcome: applyOutcome,
    failure_code: failureCode,
    verification_evidence_hash: verificationEvidenceHash,
    verification_outcome: verificationOutcome,
    created_at: timestamp(object.created_at, `${path}.created_at`),
    completed_at: completedAt,
  };
}

export function decodeWorkPatchMaterializationPageV2(
  value: unknown,
): WorkPatchMaterializationPageV2 {
  const path = "work_patch_materialization_page";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "target_branch_id",
      "source_branch_id",
      "operations",
      "next_cursor",
    ],
    path,
  );
  if (object.schema_version !== 2) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  if (!Array.isArray(object.operations) || object.operations.length > 50) {
    throw new TypeError(`${path}.operations must be a bounded array`);
  }
  const workId = resourceIdentity(object.work_id, `${path}.work_id`);
  const targetBranchId = resourceIdentity(
    object.target_branch_id,
    `${path}.target_branch_id`,
  );
  const sourceBranchId = resourceIdentity(
    object.source_branch_id,
    `${path}.source_branch_id`,
  );
  const operations = object.operations.map((entry, index) => {
    const operation = decodeWorkPatchMaterializationOperationV2(entry);
    if (
      operation.work_id !== workId ||
      operation.target_branch_id !== targetBranchId ||
      operation.source_branch_id !== sourceBranchId
    ) {
      throw new TypeError(`${path}.operations[${index}] belongs to another branch pair`);
    }
    return operation;
  });
  for (let index = 1; index < operations.length; index += 1) {
    const previous = operations[index - 1]!;
    const current = operations[index]!;
    const previousTime = utcTimestampSortKey(previous.created_at);
    const currentTime = utcTimestampSortKey(current.created_at);
    if (
      previousTime < currentTime ||
      (previousTime === currentTime && previous.operation_id <= current.operation_id)
    ) {
      throw new TypeError(`${path}.operations is not in canonical descending order`);
    }
  }
  let nextCursor: WorkPatchMaterializationPageV2["next_cursor"] = null;
  if (object.next_cursor !== null) {
    const cursor = exactObject(
      object.next_cursor,
      ["created_at", "operation_id"],
      `${path}.next_cursor`,
    );
    const createdAt = timestamp(cursor.created_at, `${path}.next_cursor.created_at`);
    const operationId = resourceIdentity(
      cursor.operation_id,
      `${path}.next_cursor.operation_id`,
    );
    const last = operations.at(-1);
    if (
      last == null ||
      last.created_at !== createdAt ||
      last.operation_id !== operationId
    ) {
      throw new TypeError(`${path}.next_cursor must identify the last returned operation`);
    }
    nextCursor = { created_at: createdAt, operation_id: operationId };
  }
  return {
    schema_version: 2,
    work_id: workId,
    target_branch_id: targetBranchId,
    source_branch_id: sourceBranchId,
    operations,
    next_cursor: nextCursor,
  };
}

export function decodeWorkPatchCommitOperationV1(
  value: unknown,
): WorkPatchCommitOperationV1 {
  const path = "work_patch_commit";
  const object = exactObject(
    value,
    [
      "schema_version",
      "operation_id",
      "work_id",
      "request_id",
      "patch_artifact_id",
      "source_branch_id",
      "target_branch_id",
      "target_branch_revision",
      "target_graph_revision",
      "base_subject_revision",
      "result_subject_revision",
      "payload_hash",
      "message",
      "provider_ref",
      "policy_decision_ref",
      "state",
      "phase",
      "commit_invocation_ref",
      "commit_sha",
      "observed_subject_revision",
      "index_reconciled",
      "failure_code",
      "created_at",
      "completed_at",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  const state = oneOf(
    object.state,
    ["pending", "aborted", "succeeded", "conflict", "failed"] as const,
    `${path}.state`,
  );
  const phase = oneOf(
    object.phase,
    ["awaiting_dispatch", "committing", "reconciling", "complete"] as const,
    `${path}.phase`,
  );
  const invocation =
    object.commit_invocation_ref === null
      ? null
      : opaqueIdentity(object.commit_invocation_ref, `${path}.commit_invocation_ref`);
  const commitSha =
    object.commit_sha === null
      ? null
      : nonEmptyString(object.commit_sha, `${path}.commit_sha`);
  if (commitSha !== null && !/^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$/u.test(commitSha)) {
    throw new TypeError(`${path}.commit_sha is not a Git object identity`);
  }
  const observedRevision =
    object.observed_subject_revision === null
      ? null
      : contentHash(object.observed_subject_revision, `${path}.observed_subject_revision`);
  let indexReconciled: boolean | null = null;
  if (object.index_reconciled !== null) {
    if (typeof object.index_reconciled !== "boolean") {
      throw new TypeError(`${path}.index_reconciled must be boolean or null`);
    }
    indexReconciled = object.index_reconciled;
  }
  const failureCode =
    object.failure_code === null
      ? null
      : oneOf(
          object.failure_code,
          [
            "authorization_denied",
            "workspace_unavailable",
            "provider_unavailable",
            "invalid_metadata",
            "base_changed",
            "result_changed",
            "patch_rejected",
            "commit_rejected",
            "ref_conflict",
          ] as const,
          `${path}.failure_code`,
        );
  const completedAt = nullableTimestamp(object.completed_at, `${path}.completed_at`);
  const hasCommitReceipt =
    commitSha !== null && observedRevision !== null && indexReconciled !== null;
  const hasNoCommitReceipt =
    commitSha === null && indexReconciled === null;
  if (
    (state === "pending" && (phase === "complete" || completedAt !== null)) ||
    (state !== "pending" && (phase !== "complete" || completedAt === null)) ||
    (phase === "awaiting_dispatch" && invocation !== null) ||
    ((phase === "committing" || phase === "reconciling") && invocation === null) ||
    (state === "pending" &&
      (commitSha !== null || observedRevision !== null || indexReconciled !== null || failureCode !== null)) ||
    (state === "aborted" &&
      (invocation !== null ||
        commitSha !== null ||
        observedRevision !== null ||
        indexReconciled !== null ||
        failureCode !== null)) ||
    (state === "succeeded" && (invocation === null || !hasCommitReceipt || failureCode !== null)) ||
    (state === "failed" &&
      (invocation === null || !hasNoCommitReceipt || failureCode === null)) ||
    (state === "conflict" &&
      (invocation === null ||
        !(
          (hasCommitReceipt && failureCode === null) ||
          (hasNoCommitReceipt && failureCode !== null)
        )))
  ) {
    throw new TypeError(`${path} lifecycle and provider receipt disagree`);
  }
  return {
    schema_version: 1,
    operation_id: resourceIdentity(object.operation_id, `${path}.operation_id`),
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    request_id: opaqueIdentity(object.request_id, `${path}.request_id`),
    patch_artifact_id: resourceIdentity(object.patch_artifact_id, `${path}.patch_artifact_id`),
    source_branch_id: resourceIdentity(object.source_branch_id, `${path}.source_branch_id`),
    target_branch_id: resourceIdentity(object.target_branch_id, `${path}.target_branch_id`),
    target_branch_revision: positiveRevision(
      object.target_branch_revision,
      `${path}.target_branch_revision`,
    ),
    target_graph_revision: positiveRevision(
      object.target_graph_revision,
      `${path}.target_graph_revision`,
    ),
    base_subject_revision: contentHash(
      object.base_subject_revision,
      `${path}.base_subject_revision`,
    ),
    result_subject_revision: contentHash(
      object.result_subject_revision,
      `${path}.result_subject_revision`,
    ),
    payload_hash: contentHash(object.payload_hash, `${path}.payload_hash`),
    message: boundedText(object.message, 4_096, `${path}.message`),
    provider_ref: opaqueIdentity(object.provider_ref, `${path}.provider_ref`),
    policy_decision_ref: opaqueIdentity(
      object.policy_decision_ref,
      `${path}.policy_decision_ref`,
    ),
    state,
    phase,
    commit_invocation_ref: invocation,
    commit_sha: commitSha,
    observed_subject_revision: observedRevision,
    index_reconciled: indexReconciled,
    failure_code: failureCode,
    created_at: timestamp(object.created_at, `${path}.created_at`),
    completed_at: completedAt,
  };
}

export function decodeWorkPatchCommitPageV1(value: unknown): WorkPatchCommitPageV1 {
  const path = "work_patch_commit_page";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "target_branch_id", "operations", "next_cursor"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  if (!Array.isArray(object.operations) || object.operations.length > 50) {
    throw new TypeError(`${path}.operations must be a bounded array`);
  }
  const workId = resourceIdentity(object.work_id, `${path}.work_id`);
  const targetBranchId = resourceIdentity(
    object.target_branch_id,
    `${path}.target_branch_id`,
  );
  const operations = object.operations.map((entry, index) => {
    const operation = decodeWorkPatchCommitOperationV1(entry);
    if (operation.work_id !== workId || operation.target_branch_id !== targetBranchId) {
      throw new TypeError(`${path}.operations[${index}] belongs to another target branch`);
    }
    return operation;
  });
  for (let index = 1; index < operations.length; index += 1) {
    const previous = operations[index - 1]!;
    const current = operations[index]!;
    const previousTime = utcTimestampSortKey(previous.created_at);
    const currentTime = utcTimestampSortKey(current.created_at);
    if (
      previousTime < currentTime ||
      (previousTime === currentTime && previous.operation_id <= current.operation_id)
    ) {
      throw new TypeError(`${path}.operations is not in canonical descending order`);
    }
  }
  let nextCursor: WorkPatchCommitPageV1["next_cursor"] = null;
  if (object.next_cursor !== null) {
    const cursor = exactObject(
      object.next_cursor,
      ["created_at", "operation_id"],
      `${path}.next_cursor`,
    );
    const createdAt = timestamp(cursor.created_at, `${path}.next_cursor.created_at`);
    const operationId = resourceIdentity(
      cursor.operation_id,
      `${path}.next_cursor.operation_id`,
    );
    const last = operations.at(-1);
    if (last == null || last.created_at !== createdAt || last.operation_id !== operationId) {
      throw new TypeError(`${path}.next_cursor must identify the last returned operation`);
    }
    nextCursor = { created_at: createdAt, operation_id: operationId };
  }
  return {
    schema_version: 1,
    work_id: workId,
    target_branch_id: targetBranchId,
    operations,
    next_cursor: nextCursor,
  };
}

/** Strict receipt for the single-transaction delivery pointer selection. */
export function decodeWorkDeliverySelectionReceiptV1(
  value: unknown,
): WorkDeliverySelectionReceiptV1 {
  const path = "work_delivery_selection";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "request_id",
      "delivery_branch_id",
      "work_revision",
      "branch_revision",
      "graph_revision",
      "evidence_manifest_hash",
      "outcome",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    request_id: opaqueIdentity(object.request_id, `${path}.request_id`),
    delivery_branch_id: resourceIdentity(
      object.delivery_branch_id,
      `${path}.delivery_branch_id`,
    ),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    branch_revision: positiveRevision(
      object.branch_revision,
      `${path}.branch_revision`,
    ),
    graph_revision: positiveRevision(object.graph_revision, `${path}.graph_revision`),
    evidence_manifest_hash: contentHash(
      object.evidence_manifest_hash,
      `${path}.evidence_manifest_hash`,
    ),
    outcome: oneOf(
      object.outcome,
      ["selected", "already_selected"] as const,
      `${path}.outcome`,
    ),
  };
}

/** Strict receipt for one atomic Work/branch retention transition. */
export function decodeWorkBranchRetentionReceiptV1(
  value: unknown,
): WorkBranchRetentionReceiptV1 {
  const path = "work_branch_retention";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "branch_id",
      "request_id",
      "kind",
      "work_revision",
      "branch_revision",
      "outcome",
    ],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path}.schema_version is unsupported`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    request_id: opaqueIdentity(object.request_id, `${path}.request_id`),
    kind: oneOf(object.kind, ["archive", "restore"] as const, `${path}.kind`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    branch_revision: positiveRevision(
      object.branch_revision,
      `${path}.branch_revision`,
    ),
    outcome: oneOf(
      object.outcome,
      ["applied", "already_in_state"] as const,
      `${path}.outcome`,
    ),
  };
}

function decodeWorkTranscriptItem(value: unknown, index: number): WorkTranscriptItemV1 {
  const path = `work_transcript.items[${index}]`;
  const object = exactObject(
    value,
    [
      "item_seq",
      "committed_turn",
      "role",
      "content",
      "content_truncated",
      "payload",
      "payload_omitted",
      "content_hash",
      "created_at",
    ],
    path,
  );
  const hash = nonEmptyString(object.content_hash, `${path}.content_hash`);
  if (!/^[0-9a-f]{64}$/u.test(hash)) {
    throw new TypeError(`${path}.content_hash must be a canonical SHA-256 hash`);
  }
  if (typeof object.content !== "string") {
    throw new TypeError(`${path}.content must be a string`);
  }
  if (typeof object.content_truncated !== "boolean" || typeof object.payload_omitted !== "boolean") {
    throw new TypeError(`${path} truncation facts must be boolean`);
  }
  if (object.payload_omitted && object.payload !== null) {
    throw new TypeError(`${path}.payload must be null when omitted`);
  }
  return {
    item_seq: safeIntegerAtLeast(object.item_seq, 1, `${path}.item_seq`),
    committed_turn: safeIntegerAtLeast(
      object.committed_turn,
      1,
      `${path}.committed_turn`,
    ),
    role: nonEmptyString(object.role, `${path}.role`),
    content: object.content,
    content_truncated: object.content_truncated,
    payload: object.payload,
    payload_omitted: object.payload_omitted,
    content_hash: hash,
    created_at: timestamp(object.created_at, `${path}.created_at`),
  };
}

/** Strict bounded page from one explicitly committed transcript prefix. */
export function decodeWorkTranscriptPageV1(value: unknown): WorkTranscriptPageV1 {
  const path = "work_transcript";
  const object = exactObject(
    value,
    [
      "schema_version",
      "work_id",
      "branch_id",
      "sync",
      "canonical_head",
      "transcript_cursor",
      "items",
      "next_before_item_seq",
      "has_more",
    ],
    path,
  );
  if (object.schema_version !== 1 || !Array.isArray(object.items) || object.items.length > 50) {
    throw new TypeError(`${path} is not a bounded v1 page`);
  }
  const sync = oneOf(
    object.sync,
    ["current", "projection_stale", "degraded", "corrupt", "offline"] as const,
    `${path}.sync`,
  );
  const canonicalHead = object.canonical_head === null
    ? null
    : decodeWorkConversationHeadAt(object.canonical_head, `${path}.canonical_head`);
  const transcriptCursor = object.transcript_cursor === null
    ? null
    : decodeWorkConversationHeadAt(object.transcript_cursor, `${path}.transcript_cursor`);
  const items = object.items.map(decodeWorkTranscriptItem);
  for (let index = 1; index < items.length; index += 1) {
    if (items[index - 1]!.item_seq >= items[index]!.item_seq) {
      throw new TypeError(`${path}.items are not in canonical order`);
    }
  }
  if (transcriptCursor === null && items.length > 0) {
    throw new TypeError(`${path}.items require a transcript cursor`);
  }
  if (
    transcriptCursor !== null &&
    items.some((item) => item.committed_turn > transcriptCursor.completed_turn)
  ) {
    throw new TypeError(`${path}.items exceed the transcript cursor`);
  }
  if (sync === "current" && !sameWorkConversationHead(canonicalHead, transcriptCursor)) {
    throw new TypeError(`${path}.current heads disagree`);
  }
  if (
    sync === "projection_stale" &&
    (canonicalHead === null ||
      (transcriptCursor !== null &&
        (transcriptCursor.completed_turn >= canonicalHead.completed_turn ||
          transcriptCursor.journal_event_seq > canonicalHead.journal_event_seq ||
          transcriptCursor.conversation_seq > canonicalHead.conversation_seq ||
          transcriptCursor.compaction_generation > canonicalHead.compaction_generation)))
  ) {
    throw new TypeError(`${path}.projection_stale cursor is not a causal prefix`);
  }
  if (sync === "corrupt" && items.length > 0) {
    throw new TypeError(`${path}.corrupt projection must not expose transcript items`);
  }
  if (typeof object.has_more !== "boolean") {
    throw new TypeError(`${path}.has_more must be a boolean`);
  }
  const next = object.next_before_item_seq === null
    ? null
    : safeIntegerAtLeast(object.next_before_item_seq, 1, `${path}.next_before_item_seq`);
  if (
    object.has_more !== (next !== null) ||
    (next !== null && next !== items[0]?.item_seq)
  ) {
    throw new TypeError(`${path}.pagination cursor is incoherent`);
  }
  return {
    schema_version: 1,
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    sync,
    canonical_head: canonicalHead,
    transcript_cursor: transcriptCursor,
    items,
    next_before_item_seq: next,
    has_more: object.has_more,
  };
}

function decodeTaskGraphBasis(value: unknown): WorkTaskGraphBasisV1 {
  const path = "work_task_graph.basis";
  const object = exactObject(
    value,
    [
      "work_id",
      "work_revision",
      "goal_revision",
      "goal",
      "criteria_set_revision",
      "criteria_member_count",
      "criteria_manifest_hash",
      "branch_id",
      "branch_revision",
      "branch_goal_revision",
      "branch_criteria_set_revision",
      "branch_basis_graph_revision",
      "graph_revision",
      "graph_item_count",
      "graph_edge_count",
      "graph_manifest_hash",
    ],
    path,
  );
  const goalRevision = positiveRevision(object.goal_revision, `${path}.goal_revision`);
  const criteriaRevision = positiveRevision(
    object.criteria_set_revision,
    `${path}.criteria_set_revision`,
  );
  const branchGoalRevision = positiveRevision(
    object.branch_goal_revision,
    `${path}.branch_goal_revision`,
  );
  const branchCriteriaRevision = positiveRevision(
    object.branch_criteria_set_revision,
    `${path}.branch_criteria_set_revision`,
  );
  const basisGraphRevision = positiveRevision(
    object.branch_basis_graph_revision,
    `${path}.branch_basis_graph_revision`,
  );
  const graphRevision = positiveRevision(object.graph_revision, `${path}.graph_revision`);
  const graphItemCount = boundedCount(
    object.graph_item_count,
    256,
    `${path}.graph_item_count`,
  );
  const graphEdgeCount = boundedCount(
    object.graph_edge_count,
    1024,
    `${path}.graph_edge_count`,
  );
  if (
    branchGoalRevision > goalRevision ||
    branchCriteriaRevision > criteriaRevision ||
    graphRevision < basisGraphRevision ||
    graphEdgeCount > (graphItemCount * (graphItemCount - 1)) / 2
  ) {
    throw new TypeError(`${path} contains incoherent causal revisions or counts`);
  }
  return {
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    goal_revision: goalRevision,
    goal: boundedText(object.goal, 16 * 1024, `${path}.goal`),
    criteria_set_revision: criteriaRevision,
    criteria_member_count: boundedCount(
      object.criteria_member_count,
      128,
      `${path}.criteria_member_count`,
    ),
    criteria_manifest_hash: contentHash(
      object.criteria_manifest_hash,
      `${path}.criteria_manifest_hash`,
    ),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    branch_revision: positiveRevision(object.branch_revision, `${path}.branch_revision`),
    branch_goal_revision: branchGoalRevision,
    branch_criteria_set_revision: branchCriteriaRevision,
    branch_basis_graph_revision: basisGraphRevision,
    graph_revision: graphRevision,
    graph_item_count: graphItemCount,
    graph_edge_count: graphEdgeCount,
    graph_manifest_hash: contentHash(
      object.graph_manifest_hash,
      `${path}.graph_manifest_hash`,
    ),
  };
}

function decodeTaskGraphCursor(value: unknown, path: string): WorkTaskGraphCursorV1 {
  const object = exactObject(
    value,
    ["graph_revision", "item_offset", "dependency_offset"],
    path,
  );
  return {
    graph_revision: positiveRevision(object.graph_revision, `${path}.graph_revision`),
    item_offset: boundedCount(object.item_offset, 256, `${path}.item_offset`),
    dependency_offset: boundedCount(
      object.dependency_offset,
      1024,
      `${path}.dependency_offset`,
    ),
  };
}

function decodeTaskGraphItem(
  value: unknown,
  index: number,
  currentGraphRevision: number,
): WorkTaskGraphItemV2 {
  const path = `work_task_graph.items.entries[${index}]`;
  const object = exactObject(
    value,
    [
      "item_id",
      "revision",
      "kind",
      "objective",
      "expected_result",
      "declaration_state",
      "execution",
      "delivery",
      "verification",
    ],
    path,
  );
  const executionObject = exactObject(
    object.execution,
    ["status", "terminal", "run"],
    `${path}.execution`,
  );
  const status = oneOf(
    executionObject.status,
    [
      "not_started",
      "running",
      "waiting",
      "paused",
      "completed",
      "delegated",
      "failed",
      "cancelled",
    ] as const,
    `${path}.execution.status`,
  );
  if (typeof executionObject.terminal !== "boolean") {
    throw new TypeError(`${path}.execution.terminal must be a boolean`);
  }
  const expectedTerminal = ["completed", "delegated", "failed", "cancelled"].includes(status);
  if (executionObject.terminal !== expectedTerminal) {
    throw new TypeError(`${path}.execution terminal state disagrees with its status`);
  }
  let run: WorkTaskGraphItemV2["execution"]["run"] = null;
  if (executionObject.run !== null) {
    const runObject = exactObject(
      executionObject.run,
      [
        "run_id",
        "attempt_id",
        "graph_revision",
        "run_generation",
        "last_event_idx",
        "updated_at",
      ],
      `${path}.execution.run`,
    );
    const runId = resourceIdentity(runObject.run_id, `${path}.execution.run.run_id`);
    const attemptId = resourceIdentity(
      runObject.attempt_id,
      `${path}.execution.run.attempt_id`,
    );
    const graphRevision = positiveRevision(
      runObject.graph_revision,
      `${path}.execution.run.graph_revision`,
    );
    if (graphRevision > currentGraphRevision) {
      throw new TypeError(`${path}.execution.run is not an admissible root item attempt`);
    }
    run = {
      run_id: runId,
      attempt_id: attemptId,
      graph_revision: graphRevision,
      run_generation: safeIntegerAtLeast(
        runObject.run_generation,
        0,
        `${path}.execution.run.run_generation`,
      ),
      last_event_idx: safeIntegerAtLeast(
        runObject.last_event_idx,
        -1,
        `${path}.execution.run.last_event_idx`,
      ),
      updated_at: timestamp(runObject.updated_at, `${path}.execution.run.updated_at`),
    };
  }
  if ((status === "not_started") !== (run === null)) {
    throw new TypeError(`${path}.execution run presence disagrees with its status`);
  }
  const execution: WorkTaskGraphItemV2["execution"] =
    status === "not_started"
      ? { status, terminal: false, run: null }
      : ["running", "waiting", "paused"].includes(status)
        ? {
            status: status as "running" | "waiting" | "paused",
            terminal: false,
            run: run!,
          }
        : {
            status: status as "completed" | "delegated" | "failed" | "cancelled",
            terminal: true,
            run: run!,
          };
  const deliveryObject = exactObject(
    object.delivery,
    ["status", "summary", "blocker_kind", "unavailable_capabilities"],
    `${path}.delivery`,
  );
  const deliveryStatus = oneOf(
    deliveryObject.status,
    ["unreported", "delivered", "blocked", "failed"] as const,
    `${path}.delivery.status`,
  );
  const deliverySummary =
    deliveryObject.summary === null
      ? null
      : boundedText(deliveryObject.summary, 8 * 1024, `${path}.delivery.summary`);
  const blockerKind =
    deliveryObject.blocker_kind === null
      ? null
      : oneOf(
          deliveryObject.blocker_kind,
          [
            "capability_unavailable",
            "dependency_blocked",
            "policy_blocked",
            "external_unavailable",
          ] as const,
          `${path}.delivery.blocker_kind`,
        );
  if (!Array.isArray(deliveryObject.unavailable_capabilities)) {
    throw new TypeError(`${path}.delivery.unavailable_capabilities must be an array`);
  }
  if (deliveryObject.unavailable_capabilities.length > 16) {
    throw new TypeError(`${path}.delivery.unavailable_capabilities exceeds its bounded size`);
  }
  const unavailableCapabilities = deliveryObject.unavailable_capabilities.map(
    (capability, index) =>
      capabilityIdentity(
        capability,
        `${path}.delivery.unavailable_capabilities[${index}]`,
      ),
  );
  if (new Set(unavailableCapabilities).size !== unavailableCapabilities.length) {
    throw new TypeError(`${path}.delivery.unavailable_capabilities contains duplicates`);
  }
  const capabilityBlocked = blockerKind === "capability_unavailable";
  if (
    (deliveryStatus === "unreported" &&
      (deliverySummary !== null || blockerKind !== null || unavailableCapabilities.length > 0)) ||
    (deliveryStatus === "blocked" &&
      (deliverySummary === null ||
        blockerKind === null ||
        capabilityBlocked !== (unavailableCapabilities.length > 0))) ||
    ((deliveryStatus === "delivered" || deliveryStatus === "failed") &&
      (deliverySummary === null || blockerKind !== null || unavailableCapabilities.length > 0))
  ) {
    throw new TypeError(`${path}.delivery facts are incoherent`);
  }
  const verificationObject = exactObject(
    object.verification,
    ["status", "latest_check"],
    `${path}.verification`,
  );
  const verificationStatus = oneOf(
    verificationObject.status,
    ["unknown", "evidence_available", "stale_evidence"] as const,
    `${path}.verification.status`,
  );
  let latestCheck: WorkTaskGraphItemV2["verification"]["latest_check"] = null;
  if (verificationObject.latest_check !== null) {
    const checkObject = exactObject(
      verificationObject.latest_check,
      [
        "check_run_id",
        "criterion",
        "criterion_set_revision",
        "graph_revision",
        "verifier_kind",
        "outcome",
        "coverage",
        "subject_revision",
        "evidence_ref_count",
        "produced_at",
        "expires_at",
        "freshness",
      ],
      `${path}.verification.latest_check`,
    );
    const criterionObject = exactObject(
      checkObject.criterion,
      ["criterion_id", "revision"],
      `${path}.verification.latest_check.criterion`,
    );
    const checkGraphRevision = positiveRevision(
      checkObject.graph_revision,
      `${path}.verification.latest_check.graph_revision`,
    );
    if (checkGraphRevision > currentGraphRevision || run === null) {
      throw new TypeError(`${path}.verification check is not on an admissible item attempt`);
    }
    const producedAt = timestamp(
      checkObject.produced_at,
      `${path}.verification.latest_check.produced_at`,
    );
    const expiresAt = nullableTimestamp(
      checkObject.expires_at,
      `${path}.verification.latest_check.expires_at`,
    );
    if (expiresAt !== null && Date.parse(expiresAt) <= Date.parse(producedAt)) {
      throw new TypeError(`${path}.verification latest check expiry is incoherent`);
    }
    const checkOutcome = oneOf(
      checkObject.outcome,
      ["passed", "failed", "error", "cancelled"] as const,
      `${path}.verification.latest_check.outcome`,
    );
    const checkCoverage = oneOf(
      checkObject.coverage,
      ["complete", "partial", "unavailable"] as const,
      `${path}.verification.latest_check.coverage`,
    );
    const evidenceRefCount = boundedCount(
      checkObject.evidence_ref_count,
      32,
      `${path}.verification.latest_check.evidence_ref_count`,
    );
    if (
      (checkOutcome === "passed" &&
        (checkCoverage !== "complete" || evidenceRefCount === 0)) ||
      (checkOutcome === "failed" && evidenceRefCount === 0)
    ) {
      throw new TypeError(`${path}.verification latest check lacks coherent coverage evidence`);
    }
    latestCheck = {
      check_run_id: resourceIdentity(
        checkObject.check_run_id,
        `${path}.verification.latest_check.check_run_id`,
      ),
      criterion: {
        criterion_id: resourceIdentity(
          criterionObject.criterion_id,
          `${path}.verification.latest_check.criterion.criterion_id`,
        ),
        revision: positiveRevision(
          criterionObject.revision,
          `${path}.verification.latest_check.criterion.revision`,
        ),
      },
      criterion_set_revision: positiveRevision(
        checkObject.criterion_set_revision,
        `${path}.verification.latest_check.criterion_set_revision`,
      ),
      graph_revision: checkGraphRevision,
      verifier_kind: oneOf(
        checkObject.verifier_kind,
        ["command", "test"] as const,
        `${path}.verification.latest_check.verifier_kind`,
      ),
      outcome: checkOutcome,
      coverage: checkCoverage,
      subject_revision: contentHash(
        checkObject.subject_revision,
        `${path}.verification.latest_check.subject_revision`,
      ),
      evidence_ref_count: evidenceRefCount,
      produced_at: producedAt,
      expires_at: expiresAt,
      freshness: oneOf(
        checkObject.freshness,
        [
          "current",
          "criteria_changed",
          "graph_changed",
          "subject_unavailable",
          "subject_changed",
          "expired",
        ] as const,
        `${path}.verification.latest_check.freshness`,
      ),
    };
  }
  const freshness = latestCheck?.freshness;
  if (
    (verificationStatus === "unknown") !== (latestCheck === null) ||
    (verificationStatus === "evidence_available") !== (freshness === "current") ||
    (verificationStatus === "stale_evidence") !==
      (latestCheck !== null && freshness !== "current")
  ) {
    throw new TypeError(`${path}.verification status disagrees with its latest check`);
  }
  return {
    item_id: resourceIdentity(object.item_id, `${path}.item_id`),
    revision: positiveRevision(object.revision, `${path}.revision`),
    kind: oneOf(object.kind, ["milestone", "task"] as const, `${path}.kind`),
    objective: boundedText(object.objective, 8 * 1024, `${path}.objective`),
    expected_result: boundedText(
      object.expected_result,
      8 * 1024,
      `${path}.expected_result`,
    ),
    declaration_state: oneOf(
      object.declaration_state,
      ["active", "superseded", "cancelled"] as const,
      `${path}.declaration_state`,
    ),
    execution,
    delivery: {
      status: deliveryStatus,
      summary: deliverySummary,
      blocker_kind: blockerKind,
      unavailable_capabilities: unavailableCapabilities,
    },
    verification: {
      status: verificationStatus,
      latest_check: latestCheck,
    },
  };
}

function decodeTaskGraphDependency(
  value: unknown,
  index: number,
): WorkTaskGraphDependencyV1 {
  const path = `work_task_graph.dependencies.entries[${index}]`;
  const object = exactObject(
    value,
    ["predecessor_item_id", "successor_item_id", "kind"],
    path,
  );
  const predecessor = resourceIdentity(
    object.predecessor_item_id,
    `${path}.predecessor_item_id`,
  );
  const successor = resourceIdentity(
    object.successor_item_id,
    `${path}.successor_item_id`,
  );
  if (predecessor === successor) {
    throw new TypeError(`${path} is self-dependent`);
  }
  return {
    predecessor_item_id: predecessor,
    successor_item_id: successor,
    kind: oneOf(object.kind, ["dependency"] as const, `${path}.kind`),
  };
}

function decodeCriteriaCursor(
  value: unknown,
  path: string,
): WorkCriteriaCursorV1 {
  const object = exactObject(value, ["criteria_set_revision", "offset"], path);
  return {
    criteria_set_revision: positiveRevision(
      object.criteria_set_revision,
      `${path}.criteria_set_revision`,
    ),
    offset: boundedCount(object.offset, 128, `${path}.offset`),
  };
}

function decodeWorkCriterion(value: unknown, index: number): WorkCriterionV1 {
  const path = `work_criteria.criteria.entries[${index}]`;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`);
  }
  const kind = oneOf(
    (value as WireObject).kind,
    ["command_check", "test_check", "human_review"] as const,
    `${path}.kind`,
  );
  const expected =
    kind === "human_review"
      ? ["criterion_id", "revision", "kind", "statement", "definition_hash"]
      : [
          "criterion_id",
          "revision",
          "kind",
          "statement",
          "command",
          "definition_hash",
        ];
  const object = exactObject(value, expected, path);
  const common = {
    criterion_id: resourceIdentity(object.criterion_id, `${path}.criterion_id`),
    revision: positiveRevision(object.revision, `${path}.revision`),
    statement: boundedText(object.statement, 16 * 1024, `${path}.statement`),
    definition_hash: contentHash(object.definition_hash, `${path}.definition_hash`),
  };
  if (kind === "human_review") {
    return { ...common, kind };
  }
  return {
    ...common,
    kind,
    command: boundedText(object.command, 64 * 1024, `${path}.command`),
  };
}

/** Strict decoder for one immutable accepted Done-when set slice. */
export function decodeWorkCriteriaPageV1(value: unknown): WorkCriteriaPageV1 {
  const path = "work_criteria";
  const object = exactObject(
    value,
    ["schema_version", "basis", "cursor", "next_cursor", "criteria"],
    path,
  );
  if (object.schema_version !== 1) {
    throw new TypeError(`${path} has an unsupported schema`);
  }
  const basisObject = exactObject(
    object.basis,
    [
      "work_id",
      "work_revision",
      "criteria_set_revision",
      "manifest_hash",
      "member_count",
    ],
    `${path}.basis`,
  );
  const basis = {
    work_id: resourceIdentity(basisObject.work_id, `${path}.basis.work_id`),
    work_revision: positiveRevision(
      basisObject.work_revision,
      `${path}.basis.work_revision`,
    ),
    criteria_set_revision: positiveRevision(
      basisObject.criteria_set_revision,
      `${path}.basis.criteria_set_revision`,
    ),
    manifest_hash: contentHash(
      basisObject.manifest_hash,
      `${path}.basis.manifest_hash`,
    ),
    member_count: boundedCount(
      basisObject.member_count,
      128,
      `${path}.basis.member_count`,
    ),
  };
  const cursor = decodeCriteriaCursor(object.cursor, `${path}.cursor`);
  const criteriaObject = exactObject(
    object.criteria,
    ["offset", "limit", "total", "entries"],
    `${path}.criteria`,
  );
  const offset = boundedCount(criteriaObject.offset, 128, `${path}.criteria.offset`);
  const limit = boundedCount(criteriaObject.limit, 8, `${path}.criteria.limit`);
  const total = boundedCount(criteriaObject.total, 128, `${path}.criteria.total`);
  if (limit === 0 || !Array.isArray(criteriaObject.entries)) {
    throw new TypeError(`${path}.criteria is not a bounded page`);
  }
  const entries = criteriaObject.entries.map(decodeWorkCriterion);
  if (offset > total) {
    throw new TypeError(`${path}.criteria offset exceeds its total`);
  }
  const expectedCount = Math.min(limit, total - offset);
  if (
    total !== basis.member_count ||
    offset !== cursor.offset ||
    cursor.criteria_set_revision !== basis.criteria_set_revision ||
    entries.length !== expectedCount ||
    entries.some(
      (entry, index) =>
        index > 0 && entries[index - 1].criterion_id >= entry.criterion_id,
    )
  ) {
    throw new TypeError(`${path} basis, cursor, and entries are incoherent`);
  }
  const expectedNextOffset = offset + entries.length;
  const nextCursor =
    object.next_cursor === null
      ? null
      : decodeCriteriaCursor(object.next_cursor, `${path}.next_cursor`);
  if (
    (expectedNextOffset < total) !== (nextCursor !== null) ||
    (nextCursor !== null &&
      (nextCursor.criteria_set_revision !== basis.criteria_set_revision ||
        nextCursor.offset !== expectedNextOffset))
  ) {
    throw new TypeError(`${path}.next_cursor is not the exact continuation`);
  }
  return {
    schema_version: 1,
    basis,
    cursor,
    next_cursor: nextCursor,
    criteria: { offset, limit, total, entries },
  };
}

const PROPOSAL_STATUSES = [
  "pending",
  "accepted",
  "rejected",
  "stale",
  "superseded",
  "expired",
] as const satisfies readonly WorkProposalStatus[];

function decodeCriteriaProposalBasis(
  value: unknown,
  path: string,
): WorkCriteriaProposalBasisV1 {
  const object = exactObject(
    value,
    [
      "work_revision",
      "goal_revision",
      "criteria_set_revision",
      "branch_revision",
      "graph_revision",
    ],
    path,
  );
  return {
    work_revision: positiveRevision(object.work_revision, `${path}.work_revision`),
    goal_revision: positiveRevision(object.goal_revision, `${path}.goal_revision`),
    criteria_set_revision: positiveRevision(
      object.criteria_set_revision,
      `${path}.criteria_set_revision`,
    ),
    branch_revision: positiveRevision(
      object.branch_revision,
      `${path}.branch_revision`,
    ),
    graph_revision: positiveRevision(object.graph_revision, `${path}.graph_revision`),
  };
}

export function decodeWorkCriteriaProposalSummaryV1(
  value: unknown,
  path = "work_criteria_proposal.proposal",
): WorkCriteriaProposalSummaryV1 {
  const object = exactObject(
    value,
    [
      "work_id",
      "branch_id",
      "proposal_id",
      "proposal_seq",
      "payload_hash",
      "status",
      "basis",
      "member_count",
      "source_kind",
      "proposed_at",
      "expires_at",
    ],
    path,
  );
  const proposedAt = timestamp(object.proposed_at, `${path}.proposed_at`);
  const expiresAt = timestamp(object.expires_at, `${path}.expires_at`);
  if (Date.parse(expiresAt) <= Date.parse(proposedAt)) {
    throw new TypeError(`${path} expiry must follow proposal creation`);
  }
  return {
    work_id: resourceIdentity(object.work_id, `${path}.work_id`),
    branch_id: resourceIdentity(object.branch_id, `${path}.branch_id`),
    proposal_id: resourceIdentity(object.proposal_id, `${path}.proposal_id`),
    proposal_seq: positiveRevision(object.proposal_seq, `${path}.proposal_seq`),
    payload_hash: contentHash(object.payload_hash, `${path}.payload_hash`),
    status: oneOf(object.status, PROPOSAL_STATUSES, `${path}.status`),
    basis: decodeCriteriaProposalBasis(object.basis, `${path}.basis`),
    member_count: boundedCount(object.member_count, 128, `${path}.member_count`),
    source_kind: oneOf(
      object.source_kind,
      ["model", "reflection"] as const,
      `${path}.source_kind`,
    ),
    proposed_at: proposedAt,
    expires_at: expiresAt,
  };
}

function decodeCriteriaProposalMember(
  value: unknown,
  index: number,
): WorkCriteriaProposalMemberV1 {
  const path = `work_criteria_proposal.members[${index}]`;
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError(`${path} must be an object`);
  }
  const memberKind = oneOf(
    (value as WireObject).member_kind,
    ["existing", "new"] as const,
    `${path}.member_kind`,
  );
  if (memberKind === "existing") {
    const object = exactObject(value, ["member_kind", "criterion_id", "revision"], path);
    return {
      member_kind: memberKind,
      criterion_id: resourceIdentity(object.criterion_id, `${path}.criterion_id`),
      revision: positiveRevision(object.revision, `${path}.revision`),
    };
  }
  const object = exactObject(value, ["member_kind", "criterion_id", "definition"], path);
  const definitionPath = `${path}.definition`;
  if (!object.definition || typeof object.definition !== "object" || Array.isArray(object.definition)) {
    throw new TypeError(`${definitionPath} must be an object`);
  }
  const kind = oneOf(
    (object.definition as WireObject).kind,
    ["command_check", "test_check", "human_review"] as const,
    `${definitionPath}.kind`,
  );
  const definitionObject = exactObject(
    object.definition,
    kind === "human_review"
      ? ["kind", "statement"]
      : ["kind", "statement", "command"],
    definitionPath,
  );
  const statement = boundedText(
    definitionObject.statement,
    16 * 1024,
    `${definitionPath}.statement`,
  );
  const definition =
    kind === "human_review"
      ? { kind, statement }
      : {
          kind,
          statement,
          command: boundedText(
            definitionObject.command,
            64 * 1024,
            `${definitionPath}.command`,
          ),
        };
  return {
    member_kind: memberKind,
    criterion_id: resourceIdentity(object.criterion_id, `${path}.criterion_id`),
    definition,
  };
}

function decodeCriteriaProposalResolution(
  value: unknown,
  path: string,
): WorkCriteriaProposalResolutionV1 {
  const object = exactObject(
    value,
    [
      "resolution_ref",
      "resolved_at",
      "result_work_revision",
      "result_criteria_set_revision",
    ],
    path,
  );
  return {
    resolution_ref: opaqueIdentity(object.resolution_ref, `${path}.resolution_ref`),
    resolved_at: timestamp(object.resolved_at, `${path}.resolved_at`),
    result_work_revision:
      object.result_work_revision === null
        ? null
        : positiveRevision(object.result_work_revision, `${path}.result_work_revision`),
    result_criteria_set_revision:
      object.result_criteria_set_revision === null
        ? null
        : positiveRevision(
            object.result_criteria_set_revision,
            `${path}.result_criteria_set_revision`,
          ),
  };
}

/** Strict decoder for one immutable provisional Done-when proposal. */
export function decodeWorkCriteriaProposalDetailV1(
  value: unknown,
): WorkCriteriaProposalDetailV1 {
  const path = "work_criteria_proposal";
  const object = exactObject(
    value,
    ["schema_version", "proposal", "members", "resolution"],
    path,
  );
  if (object.schema_version !== 1 || !Array.isArray(object.members)) {
    throw new TypeError(`${path} has an unsupported or unbounded shape`);
  }
  const proposal = decodeWorkCriteriaProposalSummaryV1(object.proposal);
  const members = object.members.map(decodeCriteriaProposalMember);
  if (
    members.length === 0 ||
    members.length > 128 ||
    members.length !== proposal.member_count ||
    members.some(
      (member, index) =>
        index > 0 && members[index - 1].criterion_id >= member.criterion_id,
    )
  ) {
    throw new TypeError(`${path}.members disagree with the canonical summary`);
  }
  const definitionBytes = members.reduce((total, member) => {
    if (member.member_kind === "existing") return total;
    return (
      total +
      new TextEncoder().encode(member.criterion_id).length +
      new TextEncoder().encode(member.definition.statement).length +
      ("command" in member.definition
        ? new TextEncoder().encode(member.definition.command).length
        : 0)
    );
  }, 0);
  if (definitionBytes > 1024 * 1024) {
    throw new TypeError(`${path}.members exceed the aggregate definition bound`);
  }
  const resolution =
    object.resolution === null
      ? null
      : decodeCriteriaProposalResolution(object.resolution, `${path}.resolution`);
  const accepted = proposal.status === "accepted";
  const pending = proposal.status === "pending";
  if (
    pending !== (resolution === null) ||
    (resolution !== null && Date.parse(resolution.resolved_at) < Date.parse(proposal.proposed_at)) ||
    (accepted &&
      (resolution?.result_work_revision !== proposal.basis.work_revision + 1 ||
        resolution.result_criteria_set_revision !==
          proposal.basis.criteria_set_revision + 1)) ||
    (!accepted &&
      !pending &&
      (resolution?.result_work_revision !== null ||
        resolution.result_criteria_set_revision !== null))
  ) {
    throw new TypeError(`${path} status and resolution are incoherent`);
  }
  return { schema_version: 1, proposal, members, resolution };
}

/** Strict decoder for the constant-cardinality pending proposal inbox. */
export function decodeWorkCriteriaProposalListV1(
  value: unknown,
): WorkCriteriaProposalListV1 {
  const path = "work_criteria_proposals";
  const object = exactObject(
    value,
    ["schema_version", "work_id", "branch_id", "proposals"],
    path,
  );
  if (object.schema_version !== 1 || !Array.isArray(object.proposals)) {
    throw new TypeError(`${path} has an unsupported shape`);
  }
  const workId = resourceIdentity(object.work_id, `${path}.work_id`);
  const branchId = resourceIdentity(object.branch_id, `${path}.branch_id`);
  const proposals = object.proposals.map((proposal, index) =>
    decodeWorkCriteriaProposalSummaryV1(proposal, `${path}.proposals[${index}]`),
  );
  if (
    proposals.length > 8 ||
    proposals.some(
      (proposal, index) =>
        proposal.status !== "pending" ||
        proposal.work_id !== workId ||
        proposal.branch_id !== branchId ||
        (index > 0 && proposals[index - 1].proposal_seq >= proposal.proposal_seq),
    ) ||
    new Set(proposals.map((proposal) => proposal.proposal_id)).size !== proposals.length
  ) {
    throw new TypeError(`${path} is not one ordered bounded pending inbox`);
  }
  return { schema_version: 1, work_id: workId, branch_id: branchId, proposals };
}

/** Strict decoder for a bounded declared-Work, Run, delivery, and Check Task Graph slice. */
export function decodeWorkTaskGraphPageV2(value: unknown): WorkTaskGraphPageV2 {
  const path = "work_task_graph";
  const object = exactObject(
    value,
    [
      "schema_version",
      "scope",
      "basis",
      "cursor",
      "next_cursor",
      "items",
      "dependencies",
    ],
    path,
  );
  if (object.schema_version !== 2 || object.scope !== "declared_work") {
    throw new TypeError(`${path} has an unsupported schema or scope`);
  }
  const basis = decodeTaskGraphBasis(object.basis);
  const cursor = decodeTaskGraphCursor(object.cursor, `${path}.cursor`);
  const itemsObject = exactObject(
    object.items,
    ["offset", "limit", "total", "entries"],
    `${path}.items`,
  );
  const dependenciesObject = exactObject(
    object.dependencies,
    ["offset", "limit", "total", "entries"],
    `${path}.dependencies`,
  );
  const itemOffset = boundedCount(itemsObject.offset, 256, `${path}.items.offset`);
  const itemLimit = boundedCount(itemsObject.limit, 8, `${path}.items.limit`);
  const itemTotal = boundedCount(itemsObject.total, 256, `${path}.items.total`);
  const dependencyOffset = boundedCount(
    dependenciesObject.offset,
    1024,
    `${path}.dependencies.offset`,
  );
  const dependencyLimit = boundedCount(
    dependenciesObject.limit,
    128,
    `${path}.dependencies.limit`,
  );
  const dependencyTotal = boundedCount(
    dependenciesObject.total,
    1024,
    `${path}.dependencies.total`,
  );
  if (itemLimit === 0 || dependencyLimit === 0) {
    throw new TypeError(`${path} page limits must be positive`);
  }
  if (!Array.isArray(itemsObject.entries) || itemsObject.entries.length > itemLimit) {
    throw new TypeError(`${path}.items.entries exceeds its page limit`);
  }
  if (
    !Array.isArray(dependenciesObject.entries) ||
    dependenciesObject.entries.length > dependencyLimit
  ) {
    throw new TypeError(`${path}.dependencies.entries exceeds its page limit`);
  }
  const items = itemsObject.entries.map((item, index) =>
    decodeTaskGraphItem(item, index, basis.graph_revision),
  );
  const dependencies = dependenciesObject.entries.map(decodeTaskGraphDependency);
  if (
    itemTotal !== basis.graph_item_count ||
    dependencyTotal !== basis.graph_edge_count ||
    itemOffset !== cursor.item_offset ||
    dependencyOffset !== cursor.dependency_offset ||
    cursor.graph_revision !== basis.graph_revision ||
    itemOffset + items.length > itemTotal ||
    dependencyOffset + dependencies.length > dependencyTotal ||
    (itemOffset < itemTotal && items.length === 0) ||
    (dependencyOffset < dependencyTotal && dependencies.length === 0) ||
    items.some((item, index) => index > 0 && items[index - 1]!.item_id >= item.item_id) ||
    dependencies.some((dependency, index) => {
      if (index === 0) return false;
      const previous = dependencies[index - 1]!;
      return `${previous.predecessor_item_id}\u0000${previous.successor_item_id}` >=
        `${dependency.predecessor_item_id}\u0000${dependency.successor_item_id}`;
    })
  ) {
    throw new TypeError(`${path} page disagrees with its pinned graph basis`);
  }
  const expectedNextItem = itemOffset + items.length;
  const expectedNextDependency = dependencyOffset + dependencies.length;
  const hasMore = expectedNextItem < itemTotal || expectedNextDependency < dependencyTotal;
  const nextCursor =
    object.next_cursor === null
      ? null
      : decodeTaskGraphCursor(object.next_cursor, `${path}.next_cursor`);
  if (
    hasMore !== (nextCursor !== null) ||
    (nextCursor !== null &&
      (nextCursor.graph_revision !== basis.graph_revision ||
        nextCursor.item_offset !== expectedNextItem ||
        nextCursor.dependency_offset !== expectedNextDependency))
  ) {
    throw new TypeError(`${path}.next_cursor disagrees with the returned page`);
  }
  return {
    schema_version: 2,
    scope: "declared_work",
    basis,
    cursor,
    next_cursor: nextCursor,
    items: {
      offset: itemOffset,
      limit: itemLimit,
      total: itemTotal,
      entries: items,
    },
    dependencies: {
      offset: dependencyOffset,
      limit: dependencyLimit,
      total: dependencyTotal,
      entries: dependencies,
    },
  };
}

const WORK_TURN_RUNTIME_EVENT_TYPES: Record<
  Exclude<
    StreamEventType,
    "session_info" | "work_turn_started" | "work_task_graph_changed"
  >,
  true
> = {
  context_meta: true,
  run_started: true,
  run_paused: true,
  run_resumed: true,
  run_finished: true,
  run_cancelled: true,
  run_waiting: true,
  run_error: true,
  run_interrupted: true,
  run_input_queued: true,
  text_delta: true,
  text_done: true,
  reasoning_delta: true,
  reasoning_done: true,
  thinking_delta: true,
  thinking_done: true,
  reasoning_message_content: true,
  tool_call: true,
  tool_call_start: true,
  tool_call_end: true,
  usage: true,
  turn_complete: true,
  error: true,
  warning: true,
  explain: true,
  plan_created: true,
  plan_revised: true,
  plan_step_start: true,
  plan_step_done: true,
  workspace_bound: true,
  executor_bound: true,
  executor_status_changed: true,
  tool_routing_decision: true,
  tool_transport_started: true,
  tool_transport_completed: true,
  tool_transport_failed: true,
  run_blocked: true,
  agent_delegated: true,
  agent_spawned: true,
  agent_live_event: true,
  agent_live_gap: true,
  stream_gap: true,
  agent_waiting: true,
  agent_progress: true,
  agent_completed: true,
  agent_failed: true,
  agent_cancelled: true,
  agent_interrupted: true,
  task_board_snapshot: true,
  tool_approval_request: true,
  ping: true,
  device_revoked: true,
  device_lease_expired: true,
  tool_execution_started: true,
  tool_output_delta: true,
  tool_execution_completed: true,
};

function containsStructuralField(value: unknown, field: string): boolean {
  if (Array.isArray(value)) {
    return value.some((child) => containsStructuralField(child, field));
  }
  if (!value || typeof value !== "object") return false;
  const object = value as WireObject;
  return (
    Object.prototype.hasOwnProperty.call(object, field) ||
    Object.values(object).some((child) => containsStructuralField(child, field))
  );
}

/** Strict identity boundary for Work continuation SSE. Runtime payloads keep
 * their established event schemas, but unknown event kinds and every
 * structural session identity fail closed. */
export function decodeWorkTurnStreamEventV1(
  value: unknown,
): WorkTurnStreamEvent {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new TypeError("work_turn_event must be an object");
  }
  if (containsStructuralField(value, "session_id")) {
    throw new TypeError("work_turn_event exposes an internal session identity");
  }
  const type = nonEmptyString((value as WireObject).type, "work_turn_event.type");
  if (type === "work_turn_started") {
    const object = exactObject(
      value,
      ["type", "schema_version", "work_id", "branch_id", "run_id"],
      "work_turn_event",
    );
    if (object.schema_version !== 1) {
      throw new TypeError("work_turn_event.schema_version is unsupported");
    }
    return {
      type: "work_turn_started",
      schema_version: 1,
      work_id: resourceIdentity(object.work_id, "work_turn_event.work_id"),
      branch_id: resourceIdentity(object.branch_id, "work_turn_event.branch_id"),
      run_id: resourceIdentity(object.run_id, "work_turn_event.run_id"),
    };
  }
  if (type === "work_task_graph_changed") {
    const object = exactObject(
      value,
      ["type", "schema_version", "graph_revision", "branch_revision"],
      "work_turn_event",
    );
    if (object.schema_version !== 1) {
      throw new TypeError("work_turn_event.schema_version is unsupported");
    }
    return {
      type: "work_task_graph_changed",
      schema_version: 1,
      graph_revision: positiveRevision(
        object.graph_revision,
        "work_turn_event.graph_revision",
      ),
      branch_revision: positiveRevision(
        object.branch_revision,
        "work_turn_event.branch_revision",
      ),
    };
  }
  if (!Object.prototype.hasOwnProperty.call(WORK_TURN_RUNTIME_EVENT_TYPES, type)) {
    throw new TypeError("work_turn_event.type is unsupported");
  }
  return value as WorkTurnStreamEvent;
}
