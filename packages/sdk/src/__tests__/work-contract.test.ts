import { readFileSync } from "node:fs";
import {
  ASTRA_WORK_API_MAJOR,
  ASTRA_WORK_API_MAJOR_HEADER,
  AstraApiError,
  AstraClient,
  type WorkEventPageV1,
  decodeWorkCatalogPageV1,
  decodeWorkArchivedBranchPageV1,
  decodeWorkBranchAttachmentV1,
  decodeWorkBranchControlOperationV2,
  decodeWorkBranchCreationOperationV1,
  decodeWorkBranchDeletionOperationV1,
  decodeWorkBranchRetentionReceiptV1,
  decodeWorkBranchCatalogV1,
  decodeWorkBranchComparisonReportV2,
  decodeWorkPatchArtifactV1,
  decodeWorkPatchArtifactPageV1,
  decodeWorkPatchMaterializationOperationV2,
  decodeWorkPatchMaterializationPageV2,
  decodeWorkPatchCommitOperationV1,
  decodeWorkPatchCommitPageV1,
  decodeWorkDeliverySelectionReceiptV1,
  decodeWorkTranscriptPageV1,
  decodeWorkObservationReportV1,
  decodeWorkCriteriaPageV1,
  decodeWorkCriteriaProposalDetailV1,
  decodeWorkCriteriaProposalListV1,
  decodeWorkEventPageV1,
  decodeWorkReadCursorReceiptV1,
  decodeWorkTurnStreamEventV1,
  reconcileWorkEventPageV1,
  decodeWorkTaskGraphPageV2,
  decodeWorkSessionBindingV1,
} from "../index";

const fixture = JSON.parse(
  readFileSync(
    new URL("../../../../fixtures/contracts/work_observation_v1.json", import.meta.url),
    "utf8",
  ),
) as unknown;

const taskGraphFixture = JSON.parse(
  readFileSync(
    new URL("../../../../fixtures/contracts/work_task_graph_v2.json", import.meta.url),
    "utf8",
  ),
) as unknown;

const criteriaPage = {
  schema_version: 1,
  basis: {
    work_id: "work-1",
    work_revision: 1,
    criteria_set_revision: 1,
    manifest_hash: `sha256:${"a".repeat(64)}`,
    member_count: 2,
  },
  cursor: { criteria_set_revision: 1, offset: 0 },
  next_cursor: { criteria_set_revision: 1, offset: 1 },
  criteria: {
    offset: 0,
    limit: 1,
    total: 2,
    entries: [
      {
        criterion_id: "review-complete",
        revision: 1,
        kind: "human_review",
        statement: "The result is reviewable.",
        definition_hash: `sha256:${"b".repeat(64)}`,
      },
    ],
  },
};

const criteriaProposalSummary = {
  work_id: "work-1",
  branch_id: "branch-1",
  proposal_id: "proposal-1",
  proposal_seq: 1,
  payload_hash: `sha256:${"c".repeat(64)}`,
  status: "pending",
  basis: {
    work_revision: 1,
    goal_revision: 1,
    criteria_set_revision: 1,
    branch_revision: 1,
    graph_revision: 1,
  },
  member_count: 1,
  source_kind: "model",
  proposed_at: "2026-08-01T00:00:00Z",
  expires_at: "2026-08-08T00:00:00Z",
} as const;

const criteriaProposalDetail = {
  schema_version: 1,
  proposal: criteriaProposalSummary,
  members: [
    {
      member_kind: "new",
      criterion_id: "tests-pass",
      definition: {
        kind: "test_check",
        statement: "Relevant tests pass.",
        command: "pnpm test",
      },
    },
  ],
  resolution: null,
} as const;

const criteriaProposalList = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "branch-1",
  proposals: [criteriaProposalSummary],
} as const;

function response(status: number, body: unknown): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status === 200 ? "OK" : "Error",
    text: () => Promise.resolve(JSON.stringify(body)),
    headers: new Headers({ "content-type": "application/json" }),
  } as unknown as Response;
}

function textResponse(data: string, headers: Record<string, string>): Response {
  return {
    ok: true,
    status: 200,
    statusText: "OK",
    text: () => Promise.resolve(data),
    headers: new Headers(headers),
  } as unknown as Response;
}

function streamResponse(events: unknown[]): Response {
  const encoder = new TextEncoder();
  const payload = events
    .map((event) => `data: ${JSON.stringify(event)}\n\n`)
    .join("");
  return {
    ok: true,
    status: 200,
    statusText: "OK",
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(encoder.encode(payload));
        controller.close();
      },
    }),
    headers: new Headers({ "content-type": "text/event-stream" }),
  } as unknown as Response;
}

let originalFetch: typeof globalThis.fetch;

beforeEach(() => {
  originalFetch = globalThis.fetch;
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

test("shared Rust/TypeScript fixture decodes with coherent causal sources", () => {
  const report = decodeWorkObservationReportV1(fixture);
  expect(report.schema_version).toBe(1);
  expect(report.overview.work_id).toBe("work-1");
  expect(report.source_revisions.map((source) => source.source)).toEqual([
    "work",
    "goal",
    "criterion_set",
    "delivery_branch",
    "graph",
    "work_events",
  ]);
});

test("decoder rejects unknown fields and cross-source revision drift", () => {
  const unknownField = structuredClone(fixture) as Record<string, unknown>;
  unknownField.legacy_session_id = "session-1";
  expect(() => decodeWorkObservationReportV1(unknownField)).toThrow(
    "unsupported field set",
  );

  const drifted = structuredClone(fixture) as {
    as_of: { graph_revision: number };
  };
  drifted.as_of.graph_revision = 2;
  expect(() => decodeWorkObservationReportV1(drifted)).toThrow(
    "disagrees with the causal cursor",
  );

  const vacuousReady = structuredClone(fixture) as {
    overview: { delivery: { status: string } };
  };
  vacuousReady.overview.delivery.status = "ready_for_review";
  expect(() => decodeWorkObservationReportV1(vacuousReady)).toThrow(
    "status disagrees",
  );

  const inventedEvidence = structuredClone(fixture) as {
    satisfaction_evidence_refs: unknown[];
  };
  inventedEvidence.satisfaction_evidence_refs.push({
    kind: "check_run",
    criterion: { criterion_id: "criterion-1", revision: 1 },
    check_run_id: "check-1",
    payload_hash: `sha256:${"a".repeat(64)}`,
  });
  expect(() => decodeWorkObservationReportV1(inventedEvidence)).toThrow(
    "evidence refs disagree",
  );

  const inventedCause = structuredClone(fixture) as {
    finding: { cause_code: string };
  };
  inventedCause.finding.cause_code = "current_evidence_complete";
  expect(() => decodeWorkObservationReportV1(inventedCause)).toThrow(
    "finding disagrees",
  );
});

test("decoder preserves exact current evidence identities for human and agent projections", () => {
  const evidenced = structuredClone(fixture) as {
    finding: { fact_code: string; cause_code: string };
    satisfaction_evidence_refs: unknown[];
    overview: {
      criteria: { member_count: number };
      delivery: {
        status: string;
        required_criterion_count: number;
        satisfied_criterion_count: number;
        fresh_check_count: number;
        accepted_gap_count: number;
        remaining_criterion_count: number;
        subject_revision: string | null;
      };
    };
  };
  evidenced.overview.criteria.member_count = 1;
  Object.assign(evidenced.overview.delivery, {
    status: "ready_for_review",
    required_criterion_count: 1,
    satisfied_criterion_count: 1,
    fresh_check_count: 1,
    accepted_gap_count: 0,
    remaining_criterion_count: 0,
    subject_revision: `sha256:${"b".repeat(64)}`,
  });
  evidenced.finding = {
    fact_code: "ready_for_review",
    cause_code: "current_evidence_complete",
  };
  evidenced.satisfaction_evidence_refs = [
    {
      kind: "check_run",
      criterion: { criterion_id: "criterion-1", revision: 1 },
      check_run_id: "check-1",
      payload_hash: `sha256:${"c".repeat(64)}`,
    },
  ];

  const report = decodeWorkObservationReportV1(evidenced);
  expect(report.finding.fact_code).toBe("ready_for_review");
  expect(report.satisfaction_evidence_refs).toEqual(
    evidenced.satisfaction_evidence_refs,
  );
});

const workCatalogPage = {
  schema_version: 1,
  entries: [
    {
      work_id: "work-2",
      goal: "Fix the failing checks.",
      work_revision: 2,
      delivery_branch_id: "branch-2",
      delivery_branch_revision: 2,
      graph_revision: 2,
      graph_item_count: 3,
      pending_decision_count: 1,
      event_head: 4,
      seen_through_event_seq: 3,
      unseen_event_count: 1,
      attention: "needs_review",
      delivery_branch_activity: "waiting",
      created_at: "2026-08-01T02:00:00.000002Z",
      last_activity_at: "2026-08-01T02:01:00.000000Z",
    },
    {
      work_id: "work-1",
      goal: "Ship a reliable change.",
      work_revision: 1,
      delivery_branch_id: "branch-1",
      delivery_branch_revision: 1,
      graph_revision: 1,
      graph_item_count: 1,
      pending_decision_count: 0,
      event_head: 2,
      seen_through_event_seq: null,
      unseen_event_count: 2,
      attention: "updated",
      delivery_branch_activity: "working",
      created_at: "2026-08-01T02:00:00.000001Z",
      last_activity_at: "2026-08-01T02:00:00.000001Z",
    },
  ],
  next_cursor: {
    created_at: "2026-08-01T02:00:00.000001Z",
    work_id: "work-1",
  },
} as const;

test("listWorks sends a stable keyset cursor and decodes server-owned attention", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, workCatalogPage));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.listWorks({
      cursor: {
        created_at: "2026-08-01T03:00:00.000000Z",
        work_id: "work-3",
      },
      limit: 2,
    }),
  ).resolves.toEqual(workCatalogPage);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works?before_created_at=2026-08-01T03%3A00%3A00.000000Z&before_work_id=work-3&limit=2",
  );
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
});

test("Work catalog rejects incoherent attention, ordering, and unbounded input", async () => {
  const incoherent = structuredClone(workCatalogPage);
  incoherent.entries[0]!.attention = "updated";
  expect(() => decodeWorkCatalogPageV1(incoherent)).toThrow(
    "attention and cursor facts disagree",
  );

  const unknownActivity = structuredClone(workCatalogPage) as {
    entries: Array<{ delivery_branch_activity: string }>;
  };
  unknownActivity.entries[0]!.delivery_branch_activity = "probably_working";
  expect(() => decodeWorkCatalogPageV1(unknownActivity)).toThrow(
    "delivery_branch_activity",
  );

  const unsorted = structuredClone(workCatalogPage);
  unsorted.entries.reverse();
  expect(() => decodeWorkCatalogPageV1(unsorted)).toThrow("canonical creation order");

  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(client.listWorks({ limit: 51 })).rejects.toThrow("between 1 and 50");
  await expect(
    client.listWorks({
      cursor: { created_at: "yesterday", work_id: "work-1" },
    }),
  ).rejects.toThrow("RFC 3339 UTC");
  expect(fetchMock).not.toHaveBeenCalled();
});

const workAttachment = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "branch-1",
  attachment_id: "attachment-1",
  attachment_epoch: 7,
  branch_revision: 4,
  mode: "read_only",
  sync: "current",
  control_basis: {
    writer_epoch: 8,
    canonical_root_hash: "a".repeat(64),
  },
  head: {
    completed_turn: 3,
    journal_event_seq: 9,
    conversation_seq: 6,
    canonical_root_hash: "a".repeat(64),
    projection_schema: 2,
    compaction_generation: 1,
    config_version_id: null,
  },
  attached_at: "2026-08-01T03:00:00Z",
  expires_at: "2026-08-01T03:15:00Z",
} as const;

test("attachWorkBranch establishes bounded read continuity without session identity", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, workAttachment));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.attachWorkBranch("work-1", "branch-1", { requestId: "open-1" }),
  ).resolves.toEqual(workAttachment);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/attachments",
  );
  expect(init.method).toBe("POST");
  expect(JSON.parse(String(init.body))).toEqual({ request_id: "open-1" });
  expect(JSON.stringify(workAttachment)).not.toContain("session_id");
});

test("detachWorkBranch releases only the exact attachment resource", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(204));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.detachWorkBranch("work-1", "branch-1", "attachment-1"),
  ).resolves.toBeUndefined();
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/attachments/attachment-1",
  );
  expect(init.method).toBe("DELETE");

  await expect(
    client.detachWorkBranch("work-1", "branch-1", "bad/attachment"),
  ).rejects.toThrow("canonical Work resource identity");
  expect(fetchMock).toHaveBeenCalledTimes(1);
});

const controlOperation = {
  schema_version: 2,
  operation_id: "operation-1",
  work_id: "work-1",
  branch_id: "branch-1",
  attachment_id: "attachment-1",
  kind: "acquire_branch_control",
  state: "succeeded",
  outcome: "acquired",
  branch_revision: 4,
  control_basis: {
    writer_epoch: 8,
    canonical_root_hash: "a".repeat(64),
  },
  created_at: "2026-08-01T03:01:00Z",
  completed_at: "2026-08-01T03:01:00Z",
} as const;

const forkHead = {
  completed_turn: 2,
  journal_event_seq: 2,
  conversation_seq: 2,
  canonical_root_hash: "b".repeat(64),
  projection_schema: 2,
  compaction_generation: 0,
  config_version_id: null,
} as const;

const forkOperation = {
  schema_version: 1,
  operation_id: "fork-operation-1",
  work_id: "work-1",
  origin_branch_id: "branch-1",
  child_branch_id: "branch-alternative-1",
  fork_cursor: `sha256:${"c".repeat(64)}`,
  state: "succeeded",
  outcome: "created",
  origin_branch_revision: 4,
  created_at: "2026-08-01T03:02:00Z",
  completed_at: "2026-08-01T03:02:01Z",
} as const;

const deletionOperation = {
  schema_version: 1,
  operation_id: "deletion-operation-1",
  work_id: "work-1",
  branch_id: "branch-alternative-1",
  state: "succeeded",
  phase: "complete",
  outcome: "deleted",
  work_revision: 5,
  branch_revision: 2,
  created_at: "2026-08-01T03:03:00Z",
  completed_at: "2026-08-01T03:03:01Z",
} as const;

const forkMaterialization = [
  { dimension: "conversation", disposition: "shared" },
  { dimension: "goal", disposition: "shared" },
  { dimension: "criteria", disposition: "shared" },
  { dimension: "task_graph", disposition: "shared" },
  { dimension: "checkpoint", disposition: "gap" },
  { dimension: "workspace", disposition: "gap" },
  { dimension: "artifacts", disposition: "gap" },
  { dimension: "transient_authority", disposition: "excluded" },
] as const;

const branchCatalog = {
  schema_version: 1,
  work_id: "work-1",
  work_revision: 4,
  delivery_branch_id: "branch-1",
  branches: [
    {
      branch_id: "branch-1",
      branch_revision: 4,
      is_delivery: true,
      origin_branch_id: null,
      fork_cursor: null,
      goal_revision_ref: 2,
      criteria_set_revision_ref: 3,
      basis_graph_revision: 1,
      current_graph_revision: 5,
      materialization: null,
      created_at: "2026-08-01T03:00:00Z",
    },
    {
      branch_id: "branch-alternative-1",
      branch_revision: 1,
      is_delivery: false,
      origin_branch_id: "branch-1",
      fork_cursor: `sha256:${"c".repeat(64)}`,
      goal_revision_ref: 2,
      criteria_set_revision_ref: 3,
      basis_graph_revision: 5,
      current_graph_revision: 5,
      materialization: forkMaterialization,
      created_at: "2026-08-01T03:02:01Z",
    },
  ],
} as const;

const branchComparison = {
  schema_version: 2,
  work_id: "work-1",
  work_revision: 4,
  directly_comparable: true,
  blockers: [],
  graph_relation: "same",
  subject_relation: "unavailable",
  evidence_relation: "same",
  left: {
    branch_id: "branch-1",
    branch_revision: 4,
    is_delivery: true,
    goal_revision_ref: 2,
    criteria: {
      revision: 3,
      manifest_hash: `sha256:${"d".repeat(64)}`,
      member_count: 2,
    },
    graph: {
      basis_revision: 1,
      current_revision: 5,
      manifest_hash: `sha256:${"e".repeat(64)}`,
      item_count: 3,
      edge_count: 2,
    },
    subject: null,
  },
  right: {
    branch_id: "branch-alternative-1",
    branch_revision: 1,
    is_delivery: false,
    goal_revision_ref: 2,
    criteria: {
      revision: 3,
      manifest_hash: `sha256:${"d".repeat(64)}`,
      member_count: 2,
    },
    graph: {
      basis_revision: 5,
      current_revision: 5,
      manifest_hash: `sha256:${"e".repeat(64)}`,
      item_count: 3,
      edge_count: 2,
    },
    subject: null,
  },
  left_evidence: {
    manifest_hash: `sha256:${"f".repeat(64)}`,
    required_count: 2,
    fresh_check_count: 0,
    accepted_gap_count: 0,
  },
  right_evidence: {
    manifest_hash: `sha256:${"f".repeat(64)}`,
    required_count: 2,
    fresh_check_count: 0,
    accepted_gap_count: 0,
  },
  coverage_gaps: ["change_details", "risks", "time_cost"],
} as const;

test("controlWorkBranch sends one sealed revision-pinned command", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(201, controlOperation));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.controlWorkBranch("work-1", "branch-1", {
      requestId: "control-1",
      expectedBranchRevision: workAttachment.branch_revision,
      expectedControlBasis: workAttachment.control_basis,
      command: { kind: "acquire_branch_control", attachmentId: "attachment-1" },
    }),
  ).resolves.toEqual(controlOperation);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/control-operations",
  );
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "control-1",
    expected_branch_revision: 4,
    expected_writer_epoch: 8,
    expected_canonical_root_hash: "a".repeat(64),
    command: { kind: "acquire_branch_control", attachment_id: "attachment-1" },
  });
  expect(JSON.stringify(controlOperation)).not.toContain("session_id");
});

test("force takeover sends step-up proof only inside the sealed command", async () => {
  const forced = {
    ...controlOperation,
    operation_id: "operation-force",
    kind: "force_takeover",
    outcome: "taken_over",
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(201, forced));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.controlWorkBranch("work-1", "branch-1", {
      requestId: "control-force-1",
      expectedBranchRevision: workAttachment.branch_revision,
      expectedControlBasis: workAttachment.control_basis,
      command: {
        kind: "force_takeover",
        attachmentId: "attachment-1",
        reauthenticationProof: "step-up-proof",
      },
    }),
  ).resolves.toEqual(forced);
  const body = JSON.parse(String((fetchMock.mock.calls[0]?.[1] as RequestInit).body));
  expect(body.command).toEqual({
    kind: "force_takeover",
    attachment_id: "attachment-1",
    reauthentication_proof: "step-up-proof",
  });
  expect(JSON.stringify(forced)).not.toContain("step-up-proof");
});

test("reauthenticate returns one bounded purpose-bound proof", async () => {
  const proof = {
    proof: "opaque-step-up-proof",
    purpose: "session_forced_takeover",
    expires_in: 300,
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(200, proof));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.reauthenticate("correct horse battery staple", "session_forced_takeover"),
  ).resolves.toEqual(proof);
  expect(fetchMock.mock.calls[0]?.[0]).toBe("https://astra.example/auth/reauthenticate");
  expect(JSON.parse(String((fetchMock.mock.calls[0]?.[1] as RequestInit).body))).toEqual({
    password: "correct horse battery staple",
    purpose: "session_forced_takeover",
  });
});

test("reauthenticate rejects a proof not bound to the requested purpose", async () => {
  globalThis.fetch = vi.fn().mockResolvedValue(
    response(200, {
      proof: "opaque-step-up-proof",
      purpose: "device_trust",
      expires_in: 300,
    }),
  );
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.reauthenticate("correct horse battery staple", "session_forced_takeover"),
  ).rejects.toThrow("reauthentication response is invalid");
});

test("Work control decoder accepts a bounded durable pending projection", () => {
  const pending = {
    ...controlOperation,
    operation_id: "operation-pending",
    kind: "force_takeover",
    state: "pending",
    outcome: "pending",
    progress: { phase: "awaiting_reauthentication", abortable: true },
    completed_at: null,
  } as const;
  expect(decodeWorkBranchControlOperationV2(pending)).toEqual(pending);

  expect(() =>
    decodeWorkBranchControlOperationV2({ ...pending, completed_at: controlOperation.completed_at }),
  ).toThrow("state and completion time disagree");
  const { progress: _, ...missingProgress } = pending;
  expect(() => decodeWorkBranchControlOperationV2(missingProgress)).toThrow(
    "work_control_operation.progress",
  );

  const aborted = {
    ...controlOperation,
    operation_id: "operation-aborted",
    kind: "force_takeover",
    state: "aborted",
    outcome: "aborted",
  } as const;
  expect(decodeWorkBranchControlOperationV2(aborted)).toEqual(aborted);
  expect(() =>
    decodeWorkBranchControlOperationV2({
      ...aborted,
      progress: { phase: "preparing", abortable: true },
    }),
  ).toThrow("progress is only valid while pending");
});

test("Work control decoder rejects contradictory terminal facts", () => {
  const contradictory = structuredClone(controlOperation);
  contradictory.state = "conflict";
  expect(() => decodeWorkBranchControlOperationV2(contradictory)).toThrow(
    "state and outcome disagree",
  );

  const badBasis = structuredClone(controlOperation);
  badBasis.control_basis.canonical_root_hash = "not-a-root";
  expect(() => decodeWorkBranchControlOperationV2(badBasis)).toThrow(
    "canonical SHA-256 hash",
  );
});

test("getWorkBranchControlOperation reads the exact durable resource", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, controlOperation));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.getWorkBranchControlOperation("work-1", "branch-1", "operation-1"),
  ).resolves.toEqual(controlOperation);
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/control-operations/operation-1",
  );
});

test("abortWorkBranchControlOperation targets only the exact durable resource", async () => {
  const fetchMock = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.abortWorkBranchControlOperation("work-1", "branch-1", "operation-1"),
  ).resolves.toBeUndefined();
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/control-operations/operation-1",
  );
  expect((fetchMock.mock.calls[0]?.[1] as RequestInit).method).toBe("DELETE");
});

test("forkWorkBranch sends one exact committed cursor without internal session identity", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(201, forkOperation));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.forkWorkBranch("work-1", "branch-1", {
      requestId: "fork-request-1",
      expectedBranchRevision: 4,
      committedCursor: forkHead,
    }),
  ).resolves.toEqual(forkOperation);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work-1/branches/branch-1/forks");
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "fork-request-1",
    expected_branch_revision: 4,
    committed_cursor: forkHead,
  });
  expect(String(init.body)).not.toContain("session_id");
});

test("Work fork decoder enforces terminal and pending fact consistency", () => {
  expect(decodeWorkBranchCreationOperationV1(forkOperation)).toEqual(forkOperation);
  const pending = {
    ...forkOperation,
    state: "pending",
    outcome: "pending",
    completed_at: null,
  } as const;
  expect(decodeWorkBranchCreationOperationV1(pending)).toEqual(pending);
  expect(() =>
    decodeWorkBranchCreationOperationV1({ ...pending, outcome: "created" }),
  ).toThrow("state and outcome disagree");
  expect(() =>
    decodeWorkBranchCreationOperationV1({ ...forkOperation, session_id: "internal" }),
  ).toThrow("unsupported field set");
});

test("forkWorkBranch rejects a non-canonical cursor before transport", async () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.forkWorkBranch("work-1", "branch-1", {
      requestId: "fork-request-invalid",
      expectedBranchRevision: 4,
      committedCursor: { ...forkHead, canonical_root_hash: "not-a-root" },
    }),
  ).rejects.toThrow("canonical SHA-256 root");
  expect(fetchMock).not.toHaveBeenCalled();
});

test("Work fork operation GET and DELETE target only the exact durable resource", async () => {
  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(200, forkOperation))
    .mockResolvedValueOnce(new Response(null, { status: 204 }));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.getWorkBranchForkOperation("work-1", "branch-1", "fork-operation-1"),
  ).resolves.toEqual(forkOperation);
  await expect(
    client.abortWorkBranchForkOperation("work-1", "branch-1", "fork-operation-1"),
  ).resolves.toBeUndefined();
  const expected =
    "https://astra.example/v1/works/work-1/branches/branch-1/forks/fork-operation-1";
  expect(fetchMock.mock.calls[0]?.[0]).toBe(expected);
  expect(fetchMock.mock.calls[1]?.[0]).toBe(expected);
  expect((fetchMock.mock.calls[1]?.[1] as RequestInit).method).toBe("DELETE");
});

test("branch deletion uses one revision-pinned durable operation", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, deletionOperation));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.deleteWorkBranch("work-1", "branch-alternative-1", {
      requestId: "delete-request-1",
      expectedWorkRevision: 4,
      expectedBranchRevision: 1,
    }),
  ).resolves.toEqual(deletionOperation);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-alternative-1/deletion-operations",
  );
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "delete-request-1",
    expected_work_revision: 4,
    expected_branch_revision: 1,
  });
  expect(String(init.body)).not.toContain("session_id");
});

test("branch deletion decoder rejects contradictory or widened facts", () => {
  expect(decodeWorkBranchDeletionOperationV1(deletionOperation)).toEqual(
    deletionOperation,
  );
  const pending = {
    ...deletionOperation,
    state: "pending",
    phase: "session_cleanup",
    outcome: "pending",
    completed_at: null,
  } as const;
  expect(decodeWorkBranchDeletionOperationV1(pending)).toEqual(pending);
  expect(() =>
    decodeWorkBranchDeletionOperationV1({ ...pending, outcome: "deleted" }),
  ).toThrow("state, phase, and outcome disagree");
  expect(() =>
    decodeWorkBranchDeletionOperationV1({ ...deletionOperation, session_id: "internal" }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkBranchDeletionOperationV1({
      ...deletionOperation,
      state: "unknown",
    }),
  ).toThrowError(TypeError);
});

test("getWorkBranchDeletionOperation observes the exact durable resource", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, deletionOperation));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.getWorkBranchDeletionOperation(
      "work-1",
      "branch-alternative-1",
      "deletion-operation-1",
    ),
  ).resolves.toEqual(deletionOperation);
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-alternative-1/deletion-operations/deletion-operation-1",
  );
});

test("listWorkBranches reads the complete bounded active catalog", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, branchCatalog));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(client.listWorkBranches("work-1")).resolves.toEqual(branchCatalog);
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches",
  );
  expect(JSON.stringify(branchCatalog)).not.toContain("session_id");
});

test("Work branch catalog rejects incomplete lineage and delivery contradictions", () => {
  expect(decodeWorkBranchCatalogV1(branchCatalog)).toEqual(branchCatalog);
  const incomplete = structuredClone(branchCatalog);
  incomplete.branches[1]!.fork_cursor = null;
  expect(() => decodeWorkBranchCatalogV1(incomplete)).toThrow("incomplete fork lineage");
  const contradictory = structuredClone(branchCatalog);
  contradictory.branches[1]!.is_delivery = true;
  expect(() => decodeWorkBranchCatalogV1(contradictory)).toThrow(
    "contradictory delivery branch identity",
  );
  const reordered = structuredClone(branchCatalog);
  reordered.branches[1]!.materialization!.reverse();
  expect(() => decodeWorkBranchCatalogV1(reordered)).toThrow(
    "canonical dimension order",
  );
});

test("compareWorkBranches sends exact identities and preserves explicit coverage gaps", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, branchComparison));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.compareWorkBranches("work-1", "branch-1", "branch-alternative-1"),
  ).resolves.toEqual(branchComparison);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work-1/branch-comparisons");
  expect(JSON.parse(String(init.body))).toEqual({
    left_branch_id: "branch-1",
    right_branch_id: "branch-alternative-1",
  });
});

test("Work branch comparison rejects contradictory and non-canonical facts", () => {
  expect(decodeWorkBranchComparisonReportV2(branchComparison)).toEqual(branchComparison);
  expect(() =>
    decodeWorkBranchComparisonReportV2({
      ...branchComparison,
      directly_comparable: false,
    }),
  ).toThrow("contradicts blockers");
  expect(() =>
    decodeWorkBranchComparisonReportV2({
      ...branchComparison,
      coverage_gaps: ["time_cost", "change_details"],
    }),
  ).toThrow("canonically ordered");
  expect(() =>
    decodeWorkBranchComparisonReportV2({
      ...branchComparison,
      graph_relation: "different",
    }),
  ).toThrow("contradicts graph facts");
  expect(() =>
    decodeWorkBranchComparisonReportV2({
      ...branchComparison,
      right: {
        ...branchComparison.right,
        graph: { ...branchComparison.right.graph, item_count: 4 },
      },
    }),
  ).toThrow("contradictory graph manifest counts");
  expect(() =>
    decodeWorkBranchComparisonReportV2({
      ...branchComparison,
      right_evidence: {
        ...branchComparison.right_evidence,
        fresh_check_count: 1,
      },
    }),
  ).toThrow("evidence_relation contradicts evidence facts");
});

test("getWorkPatchArtifact reads strict Work provenance without session identity", async () => {
  const artifact = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    patch_artifact_id: "patch-1",
    source_branch_revision: 4,
    source_graph_revision: 3,
    base_subject_revision: `sha256:${"a".repeat(64)}`,
    result_subject_revision: `sha256:${"b".repeat(64)}`,
    payload_hash: `sha256:${"c".repeat(64)}`,
    payload_bytes: 42,
    format: "unified_diff_v1",
    provider_invocation_ref: "invocation-1",
    source_ref: "event-1",
    created_at: "2026-08-02T12:00:00.000000Z",
  } as const;
  expect(decodeWorkPatchArtifactV1(artifact)).toEqual(artifact);
  expect(() =>
    decodeWorkPatchArtifactV1({ ...artifact, session_id: "internal-session" }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkPatchArtifactV1({ ...artifact, payload_artifact_id: "internal-payload" }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkPatchArtifactV1({ ...artifact, subject_ref: "internal-workspace" }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkPatchArtifactV1({ ...artifact, payload_bytes: 16 * 1024 * 1024 + 1 }),
  ).toThrow("between 0 and");

  const fetchMock = vi.fn().mockImplementation(() =>
    Promise.resolve(response(200, artifact)),
  );
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(
    client.getWorkPatchArtifact("work-1", "branch-1", "patch-1"),
  ).resolves.toEqual(artifact);
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-artifacts/patch-1",
  );
  fetchMock.mockClear();
  await expect(
    client.exportWorkPatchArtifact("work-1", "branch-1", {
      requestId: "event-1",
      expectedBranchRevision: 4,
      expectedGraphRevision: 3,
    }),
  ).resolves.toEqual(artifact);
  const [exportUrl, exportInit] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(exportUrl).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-artifacts",
  );
  expect(JSON.parse(String(exportInit.body))).toEqual({
    request_id: "event-1",
    expected_branch_revision: 4,
    expected_graph_revision: 3,
  });

  const diff = "diff --git a/file b/file\n-old\n+new\n";
  const diffHash = `sha256:${"d".repeat(64)}` as const;
  fetchMock.mockResolvedValueOnce(
    textResponse(diff, {
      "content-type": "text/x-diff; charset=utf-8",
      "content-length": String(new TextEncoder().encode(diff).byteLength),
      etag: `"${diffHash}"`,
    }),
  );
  await expect(
    client.getWorkPatchArtifactContent("work-1", "branch-1", "patch-1"),
  ).resolves.toEqual({
    data: diff,
    hash: diffHash,
    bytes: new TextEncoder().encode(diff).byteLength,
  });
  const [contentUrl, contentInit] = fetchMock.mock.calls.at(-1) as [
    string,
    RequestInit,
  ];
  expect(contentUrl).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-artifacts/patch-1/content",
  );
  expect(new Headers(contentInit.headers).get(ASTRA_WORK_API_MAJOR_HEADER)).toBe(
    ASTRA_WORK_API_MAJOR,
  );

  fetchMock.mockResolvedValueOnce(
    textResponse(diff, {
      "content-type": "text/x-diff",
      "content-length": String(new TextEncoder().encode(diff).byteLength + 1),
      etag: `"${diffHash}"`,
    }),
  );
  await expect(
    client.getWorkPatchArtifactContent("work-1", "branch-1", "patch-1"),
  ).rejects.toThrow("length disagrees");

  const page = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    artifacts: [artifact],
    next_cursor: {
      created_at: artifact.created_at,
      patch_artifact_id: artifact.patch_artifact_id,
    },
  } as const;
  expect(decodeWorkPatchArtifactPageV1(page)).toEqual(page);
  expect(() =>
    decodeWorkPatchArtifactPageV1({
      ...page,
      next_cursor: { ...page.next_cursor, patch_artifact_id: "another-patch" },
    }),
  ).toThrow("last returned artifact");
  fetchMock.mockResolvedValueOnce(response(200, page));
  await expect(
    client.listWorkPatchArtifacts("work-1", "branch-1", {
      before: page.next_cursor,
      limit: 1,
    }),
  ).resolves.toEqual(page);
  expect(fetchMock.mock.calls.at(-1)?.[0]).toBe(
    `https://astra.example/v1/works/work-1/branches/branch-1/patch-artifacts?before_created_at=${encodeURIComponent(artifact.created_at)}&before_patch_artifact_id=patch-1&limit=1`,
  );
  await expect(
    client.listWorkPatchArtifacts("work-1", "branch-1", { limit: 51 }),
  ).rejects.toThrow("between 1 and 50");
});

test("patch materialization uses exact target facts and hides executor authority", async () => {
  const operation = {
    schema_version: 2,
    operation_id: "materialization-1",
    work_id: "work-1",
    request_id: "materialize-request-1",
    patch_artifact_id: "patch-1",
    source_branch_id: "branch-alternative-1",
    target_branch_id: "branch-1",
    target_branch_revision: 4,
    target_graph_revision: 3,
    base_subject_revision: `sha256:${"a".repeat(64)}`,
    result_subject_revision: `sha256:${"b".repeat(64)}`,
    payload_hash: `sha256:${"c".repeat(64)}`,
    provider_ref: "edge://workspace-1",
    policy_decision_ref: "policy-decision-1",
    state: "pending",
    phase: "awaiting_dispatch",
    apply_invocation_ref: null,
    observed_subject_revision: null,
    apply_outcome: null,
    failure_code: null,
    verification_evidence_hash: null,
    verification_outcome: null,
    created_at: "2026-08-02T12:00:00.000000Z",
    completed_at: null,
  } as const;
  expect(decodeWorkPatchMaterializationOperationV2(operation)).toEqual(operation);
  expect(() =>
    decodeWorkPatchMaterializationOperationV2({
      ...operation,
      executor_token: "internal-executor",
    }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkPatchMaterializationOperationV2({
      ...operation,
      subject_ref: "internal/workspace/ref",
      target_subject_record_revision: 9,
    }),
  ).toThrow("unsupported field set");
  expect(() =>
    decodeWorkPatchMaterializationOperationV2({
      ...operation,
      phase: "verifying",
    }),
  ).toThrow("apply outcome contradicts lifecycle state");
  const verified = {
    ...operation,
    state: "succeeded",
    phase: "complete",
    apply_invocation_ref: "apply-invocation-1",
    observed_subject_revision: operation.result_subject_revision,
    apply_outcome: "applied",
    verification_evidence_hash: `sha256:${"d".repeat(64)}`,
    verification_outcome: "passed",
    completed_at: "2026-08-02T12:01:00.000000Z",
  } as const;
  expect(decodeWorkPatchMaterializationOperationV2(verified)).toEqual(verified);
  const reconciling = {
    ...operation,
    phase: "reconciling",
    apply_invocation_ref: "apply-invocation-1",
  } as const;
  expect(decodeWorkPatchMaterializationOperationV2(reconciling)).toEqual(reconciling);
  const notApplied = {
    ...operation,
    state: "failed",
    phase: "complete",
    apply_invocation_ref: "apply-invocation-2",
    apply_outcome: "not_applied",
    failure_code: "patch_rejected",
    completed_at: "2026-08-02T12:01:00.000000Z",
  } as const;
  expect(decodeWorkPatchMaterializationOperationV2(notApplied)).toEqual(notApplied);
  expect(() =>
    decodeWorkPatchMaterializationOperationV2({
      ...notApplied,
      failure_code: null,
    }),
  ).toThrow("apply outcome contradicts lifecycle state");

  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(202, operation))
    .mockResolvedValueOnce(
      response(200, {
        schema_version: 2,
        work_id: "work-1",
        target_branch_id: "branch-1",
        source_branch_id: "branch-alternative-1",
        operations: [operation],
        next_cursor: null,
      }),
    )
    .mockResolvedValueOnce(response(200, operation))
    .mockResolvedValueOnce(response(204, undefined));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  const input = {
    requestId: "materialize-request-1",
    patchArtifactId: "patch-1",
    expectedTargetBranchRevision: 4,
    expectedTargetGraphRevision: 3,
  };
  await expect(
    client.materializeWorkPatch("work-1", "branch-1", input),
  ).resolves.toEqual(operation);
  await expect(
    client.listWorkPatchMaterializations("work-1", "branch-1", {
      sourceBranchId: "branch-alternative-1",
      limit: 10,
    }),
  ).resolves.toMatchObject({ operations: [operation], next_cursor: null });
  await expect(
    client.getWorkPatchMaterialization("work-1", "branch-1", "materialization-1"),
  ).resolves.toEqual(operation);
  await expect(
    client.abortWorkPatchMaterialization("work-1", "branch-1", "materialization-1"),
  ).resolves.toBeUndefined();
  const [postUrl, postInit] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(postUrl).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-materializations",
  );
  expect(JSON.parse(String(postInit.body))).toEqual({
    request_id: input.requestId,
    patch_artifact_id: input.patchArtifactId,
    expected_target_branch_revision: input.expectedTargetBranchRevision,
    expected_target_graph_revision: input.expectedTargetGraphRevision,
  });
  expect(fetchMock.mock.calls[1]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-materializations?source_branch_id=branch-alternative-1&limit=10",
  );
  expect(fetchMock.mock.calls[2]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-materializations/materialization-1",
  );
  expect(fetchMock.mock.calls[3]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-materializations/materialization-1",
  );
  expect(fetchMock.mock.calls[3]?.[1]).toMatchObject({ method: "DELETE" });
  expect(() =>
    decodeWorkPatchMaterializationPageV2({
      schema_version: 2,
      work_id: "work-1",
      target_branch_id: "branch-1",
      source_branch_id: "branch-alternative-1",
      operations: [operation],
      next_cursor: {
        created_at: operation.created_at,
        operation_id: "another-operation",
      },
    }),
  ).toThrow("must identify the last returned operation");
});

test("reviewed patch commit is durable, strict, and keeps identity server-owned", async () => {
  const operation = {
    schema_version: 1,
    operation_id: "commit-1",
    work_id: "work-1",
    request_id: "commit-request-1",
    patch_artifact_id: "patch-1",
    source_branch_id: "branch-1",
    target_branch_id: "branch-1",
    target_branch_revision: 4,
    target_graph_revision: 3,
    base_subject_revision: `sha256:${"a".repeat(64)}`,
    result_subject_revision: `sha256:${"b".repeat(64)}`,
    payload_hash: `sha256:${"c".repeat(64)}`,
    message: "Commit reviewed changes",
    provider_ref: "server-git-worktree-commit-v1",
    policy_decision_ref: "commit-request-1",
    state: "pending",
    phase: "awaiting_dispatch",
    commit_invocation_ref: null,
    commit_sha: null,
    observed_subject_revision: null,
    index_reconciled: null,
    failure_code: null,
    created_at: "2026-08-02T12:00:00.000000Z",
    completed_at: null,
  } as const;
  expect(decodeWorkPatchCommitOperationV1(operation)).toEqual(operation);
  for (const internal of ["author_email", "author_name", "executor_token"] as const) {
    expect(() =>
      decodeWorkPatchCommitOperationV1({ ...operation, [internal]: "forged" }),
    ).toThrow("unsupported field set");
  }
  expect(() =>
    decodeWorkPatchCommitOperationV1({
      ...operation,
      phase: "committing",
      commit_invocation_ref: null,
    }),
  ).toThrow("lifecycle and provider receipt disagree");
  const succeeded = {
    ...operation,
    state: "succeeded",
    phase: "complete",
    commit_invocation_ref: "server-git-commit:commit-1",
    commit_sha: "d".repeat(40),
    observed_subject_revision: `sha256:${"e".repeat(64)}`,
    index_reconciled: true,
    completed_at: "2026-08-02T12:01:00.000000Z",
  } as const;
  expect(decodeWorkPatchCommitOperationV1(succeeded)).toEqual(succeeded);
  expect(() =>
    decodeWorkPatchCommitOperationV1({ ...succeeded, commit_sha: null }),
  ).toThrow("lifecycle and provider receipt disagree");

  const page = {
    schema_version: 1,
    work_id: "work-1",
    target_branch_id: "branch-1",
    operations: [operation],
    next_cursor: null,
  } as const;
  expect(decodeWorkPatchCommitPageV1(page)).toEqual(page);
  expect(() =>
    decodeWorkPatchCommitPageV1({
      ...page,
      next_cursor: { created_at: operation.created_at, operation_id: "wrong" },
    }),
  ).toThrow("must identify the last returned operation");

  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(202, operation))
    .mockResolvedValueOnce(response(200, page))
    .mockResolvedValueOnce(response(200, operation))
    .mockResolvedValueOnce(response(204, undefined));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  const input = {
    requestId: "commit-request-1",
    patchArtifactId: "patch-1",
    expectedTargetBranchRevision: 4,
    expectedTargetGraphRevision: 3,
    message: "Commit reviewed changes",
  };
  await expect(client.commitWorkPatch("work-1", "branch-1", input)).resolves.toEqual(
    operation,
  );
  await expect(
    client.listWorkPatchCommits("work-1", "branch-1", { limit: 10 }),
  ).resolves.toEqual(page);
  await expect(client.getWorkPatchCommit("work-1", "branch-1", "commit-1")).resolves.toEqual(
    operation,
  );
  await expect(
    client.abortWorkPatchCommit("work-1", "branch-1", "commit-1"),
  ).resolves.toBeUndefined();
  const [postUrl, postInit] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(postUrl).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-commits",
  );
  expect(JSON.parse(String(postInit.body))).toEqual({
    request_id: input.requestId,
    patch_artifact_id: input.patchArtifactId,
    expected_target_branch_revision: input.expectedTargetBranchRevision,
    expected_target_graph_revision: input.expectedTargetGraphRevision,
    message: input.message,
  });
  expect(fetchMock.mock.calls[1]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/patch-commits?limit=10",
  );
  expect(fetchMock.mock.calls[3]?.[1]).toMatchObject({ method: "DELETE" });
  await expect(
    client.commitWorkPatch("work-1", "branch-1", { ...input, message: "\u0000" }),
  ).rejects.toThrow("message must be non-empty");
});

test("selectWorkDeliveryBranch sends the complete comparison basis and seals its receipt", async () => {
  const receipt = {
    schema_version: 1,
    work_id: "work-1",
    request_id: "select-result-1",
    delivery_branch_id: "branch-alternative-1",
    work_revision: 5,
    branch_revision: 1,
    graph_revision: 5,
    evidence_manifest_hash: branchComparison.right_evidence.manifest_hash,
    outcome: "selected",
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(200, receipt));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.selectWorkDeliveryBranch("work-1", {
      requestId: "select-result-1",
      branchId: branchComparison.right.branch_id,
      expectedWorkRevision: branchComparison.work_revision,
      expectedBranchRevision: branchComparison.right.branch_revision,
      expectedGoalRevision: branchComparison.right.goal_revision_ref,
      expectedCriteriaSetRevision: branchComparison.right.criteria.revision,
      expectedGraphRevision: branchComparison.right.graph.current_revision,
      expectedSubject: null,
      expectedEvidenceManifestHash: branchComparison.right_evidence.manifest_hash,
    }),
  ).resolves.toEqual(receipt);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work-1/actions");
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "select-result-1",
    expected_work_revision: 4,
    action: {
      kind: "select_delivery_branch",
      branch_id: "branch-alternative-1",
      expected_branch_revision: 1,
      expected_goal_revision: 2,
      expected_criteria_set_revision: 3,
      expected_graph_revision: 5,
      expected_subject: null,
      expected_evidence_manifest_hash: `sha256:${"f".repeat(64)}`,
    },
  });
  expect(decodeWorkDeliverySelectionReceiptV1(receipt)).toEqual(receipt);
  expect(() =>
    decodeWorkDeliverySelectionReceiptV1({ ...receipt, branch_id: "shadow" }),
  ).toThrow("unsupported field set");
});

test("branch retention uses a branch-scoped pinned command and seals transition revisions", async () => {
  const archived = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-alternative-1",
    request_id: "archive-branch-1",
    kind: "archive",
    work_revision: 5,
    branch_revision: 3,
    outcome: "applied",
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(200, archived));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.archiveWorkBranch("work-1", "branch-alternative-1", {
      requestId: "archive-branch-1",
      expectedWorkRevision: 4,
      expectedBranchRevision: 2,
    }),
  ).resolves.toEqual(archived);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-alternative-1/actions",
  );
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "archive-branch-1",
    expected_work_revision: 4,
    expected_branch_revision: 2,
    action: { kind: "archive" },
  });
  expect(decodeWorkBranchRetentionReceiptV1(archived)).toEqual(archived);

  fetchMock.mockResolvedValueOnce(
    response(200, { ...archived, work_revision: 4 }),
  );
  await expect(
    client.archiveWorkBranch("work-1", "branch-alternative-1", {
      requestId: "archive-branch-1",
      expectedWorkRevision: 4,
      expectedBranchRevision: 2,
    }),
  ).rejects.toThrow("receipt disagrees with the request");
  expect(() =>
    decodeWorkBranchRetentionReceiptV1({ ...archived, archived_at: "surprise" }),
  ).toThrow("unsupported field set");
});

test("archived branch history is cursor-bounded and canonically ordered", async () => {
  const page = {
    schema_version: 1,
    work_id: "work-1",
    work_revision: 9,
    branches: [
      {
        branch_id: "branch-2",
        branch_revision: 3,
        origin_branch_id: "branch-1",
        archived_at: "2026-08-02T02:00:00.000000Z",
        created_at: "2026-08-01T02:00:00.000000Z",
      },
      {
        branch_id: "branch-1",
        branch_revision: 4,
        origin_branch_id: null,
        archived_at: "2026-08-02T01:00:00.000000Z",
        created_at: "2026-07-31T02:00:00.000000Z",
      },
    ],
    next_cursor: {
      archived_at: "2026-08-02T01:00:00.000000Z",
      branch_id: "branch-1",
    },
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(200, page));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(
    client.listArchivedWorkBranches("work-1", {
      before: {
        archived_at: "2026-08-03T00:00:00.000000Z",
        branch_id: "branch-9",
      },
      limit: 2,
    }),
  ).resolves.toEqual(page);
  expect(fetchMock.mock.calls[0]![0]).toBe(
    "https://astra.example/v1/works/work-1/branches/archived" +
      "?before_archived_at=2026-08-03T00%3A00%3A00.000000Z&before_branch_id=branch-9&limit=2",
  );
  expect(decodeWorkArchivedBranchPageV1(page)).toEqual(page);
  expect(() =>
    decodeWorkArchivedBranchPageV1({
      ...page,
      branches: [...page.branches].reverse(),
    }),
  ).toThrow("canonical archive order");
  expect(() =>
    decodeWorkArchivedBranchPageV1({ ...page, next_cursor: null }),
  ).not.toThrow();
  expect(() =>
    decodeWorkArchivedBranchPageV1({
      ...page,
      next_cursor: { ...page.next_cursor, branch_id: "branch-2" },
    }),
  ).toThrow("does not seal the last branch");
});

const transcriptHead = {
  completed_turn: 2,
  journal_event_seq: 2,
  conversation_seq: 2,
  canonical_root_hash: "b".repeat(64),
  projection_schema: 2,
  compaction_generation: 0,
  config_version_id: null,
} as const;

const transcriptPage = {
  schema_version: 1,
  work_id: "work-1",
  branch_id: "branch-1",
  sync: "current",
  canonical_head: transcriptHead,
  transcript_cursor: transcriptHead,
  items: [
    {
      item_seq: 7,
      committed_turn: 2,
      role: "user",
      content: "continue",
      content_truncated: false,
      payload: null,
      payload_omitted: false,
      content_hash: "c".repeat(64),
      created_at: "2026-08-01T03:00:00.000000Z",
    },
  ],
  next_before_item_seq: 7,
  has_more: true,
} as const;

test("getWorkBranchTranscript reads one bounded committed page", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, transcriptPage));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.getWorkBranchTranscript("work-1", "branch-1", {
      beforeItemSeq: 9,
      limit: 50,
    }),
  ).resolves.toEqual(transcriptPage);
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/transcript?before_item_seq=9&limit=50",
  );
  expect(JSON.stringify(transcriptPage)).not.toContain("session_id");
});

test("Work transcript decoder fails closed on incoherent causal or pagination facts", () => {
  const stale = structuredClone(transcriptPage);
  stale.sync = "projection_stale";
  stale.canonical_head = {
    ...stale.canonical_head,
    completed_turn: 3,
    journal_event_seq: 3,
    conversation_seq: 3,
    canonical_root_hash: "d".repeat(64),
  };
  expect(decodeWorkTranscriptPageV1(stale).sync).toBe("projection_stale");

  const falseCurrent = structuredClone(stale);
  falseCurrent.sync = "current";
  expect(() => decodeWorkTranscriptPageV1(falseCurrent)).toThrow("current heads disagree");

  const uncommitted = structuredClone(transcriptPage);
  uncommitted.items[0].committed_turn = 3;
  expect(() => decodeWorkTranscriptPageV1(uncommitted)).toThrow("exceed the transcript cursor");

  const incoherentPage = structuredClone(transcriptPage);
  incoherentPage.next_before_item_seq = 8;
  expect(() => decodeWorkTranscriptPageV1(incoherentPage)).toThrow("pagination cursor");
});

test("Work attachment rejects malformed continuity and request identities", async () => {
  const internalIdentity = structuredClone(workAttachment) as Record<string, unknown>;
  internalIdentity.session_id = "internal-session";
  expect(() => decodeWorkBranchAttachmentV1(internalIdentity)).toThrow(
    "unsupported field set",
  );

  const malformedHead = structuredClone(workAttachment);
  malformedHead.head.canonical_root_hash = "not-a-root";
  expect(() => decodeWorkBranchAttachmentV1(malformedHead)).toThrow(
    "canonical SHA-256 root",
  );

  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(
    client.attachWorkBranch("work-1", "branch-1", { requestId: "bad\nrequest" }),
  ).rejects.toThrow("control-free");
  expect(fetchMock).not.toHaveBeenCalled();
});

test("createWork sends one strict idempotent Start Work command", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(201, fixture));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({
    baseUrl: "https://astra.example",
    accessToken: "token",
  });

  const report = await client.createWork({
    requestId: "start-work-1",
    goal: "Ship the typed Work creation boundary.",
    criteria: [
      {
        criterionId: "tests-pass",
        kind: "test_check",
        statement: "Relevant tests pass.",
        command: "npm test",
      },
      {
        criterionId: "review-complete",
        kind: "human_review",
        statement: "The result is reviewable.",
      },
    ],
  });
  expect(report.overview.work_id).toBe("work-1");
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works");
  expect(init.method).toBe("POST");
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "start-work-1",
    goal: "Ship the typed Work creation boundary.",
    criteria: [
      {
        criterion_id: "review-complete",
        kind: "human_review",
        statement: "The result is reviewable.",
      },
      {
        criterion_id: "tests-pass",
        kind: "test_check",
        statement: "Relevant tests pass.",
        command: "npm test",
      },
    ],
  });
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
});

test("createWork rejects ambiguous identity and oversized goals before transport", async () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.createWork({ requestId: "", goal: "A real goal", criteria: [] }),
  ).rejects.toThrow("requestId");
  await expect(
    client.createWork({ requestId: "bad\u0085request", goal: "A real goal", criteria: [] }),
  ).rejects.toThrow("requestId");
  await expect(
    client.createWork({ requestId: "界".repeat(86), goal: "A real goal", criteria: [] }),
  ).rejects.toThrow("256 UTF-8 bytes");
  await expect(
    client.createWork({ requestId: "start-work-1", goal: "x".repeat(8193), criteria: [] }),
  ).rejects.toThrow("8192 UTF-8 bytes");
  expect(fetchMock).not.toHaveBeenCalled();
});

test("createWork rejects incoherent or unbounded criteria before transport", async () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  const base = { requestId: "start-work-criteria", goal: "Ship it." };

  await expect(
    client.createWork({
      ...base,
      criteria: [
        { criterionId: "same", kind: "human_review", statement: "First." },
        { criterionId: "same", kind: "human_review", statement: "Second." },
      ],
    }),
  ).rejects.toThrow("repeats criterionId");
  await expect(
    client.createWork({
      ...base,
      criteria: [
        { criterionId: "../unsafe", kind: "human_review", statement: "Review." },
      ],
    }),
  ).rejects.toThrow("safe resource identity");
  await expect(
    client.createWork({
      ...base,
      criteria: Array.from({ length: 16 }, (_, index) => ({
        criterionId: `criterion-${index}`,
        kind: "command_check" as const,
        statement: "Bound the request.",
        command: "x".repeat(64 * 1024),
      })),
    }),
  ).rejects.toThrow("1048576 UTF-8 bytes");
  expect(fetchMock).not.toHaveBeenCalled();
});

test("getWorkCriteria reads one strict revision-pinned Done-when page", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, criteriaPage));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  const page = await client.getWorkCriteria("work-1", { limit: 1 });
  expect(page.criteria.entries[0].criterion_id).toBe("review-complete");
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work-1/criteria?limit=1");
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );

  const torn = structuredClone(criteriaPage);
  torn.next_cursor = null as unknown as { criteria_set_revision: number; offset: number };
  expect(() => decodeWorkCriteriaPageV1(torn)).toThrow("next_cursor");
  const unsupported = structuredClone(criteriaPage) as {
    criteria: { entries: Array<{ kind: string }> };
  };
  unsupported.criteria.entries[0].kind = "model_assessment";
  expect(() => decodeWorkCriteriaPageV1(unsupported)).toThrow("unsupported value");
});

test("criteria proposal inbox, detail, and exact decision share one typed contract", async () => {
  const accepted = structuredClone(criteriaProposalDetail) as Record<string, unknown> & {
    proposal: { status: string };
    resolution: unknown;
  };
  accepted.proposal.status = "accepted";
  accepted.resolution = {
    resolution_ref: "criteria-decision-1",
    resolved_at: "2026-08-01T00:01:00Z",
    result_work_revision: 2,
    result_criteria_set_revision: 2,
  };
  const fetchMock = vi
    .fn()
    .mockResolvedValueOnce(response(200, criteriaProposalList))
    .mockResolvedValueOnce(response(200, criteriaProposalDetail))
    .mockResolvedValueOnce(response(200, accepted));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  const inbox = await client.listWorkCriteriaProposals("work-1", "branch-1");
  const detail = await client.getWorkCriteriaProposal(
    "work-1",
    "branch-1",
    "proposal-1",
  );
  const resolved = await client.resolveWorkCriteriaProposal(
    "work-1",
    "branch-1",
    detail.proposal,
    { requestId: "accept-1", decision: "accept" },
  );
  expect(inbox.proposals).toHaveLength(1);
  expect(resolved.proposal.status).toBe("accepted");
  expect(fetchMock.mock.calls.map(([url]) => url)).toEqual([
    "https://astra.example/v1/works/work-1/branches/branch-1/criteria-proposals",
    "https://astra.example/v1/works/work-1/branches/branch-1/criteria-proposals/proposal-1",
    "https://astra.example/v1/works/work-1/branches/branch-1/criteria-proposals/proposal-1/decision",
  ]);
  const decisionInit = fetchMock.mock.calls[2][1] as RequestInit;
  expect(decisionInit.method).toBe("PUT");
  expect(JSON.parse(String(decisionInit.body))).toEqual({
    request_id: "accept-1",
    decision: "accept",
    payload_hash: criteriaProposalSummary.payload_hash,
    expected_work_revision: 1,
    expected_goal_revision: 1,
    expected_criteria_set_revision: 1,
    expected_branch_revision: 1,
    expected_graph_revision: 1,
  });
});

test("criteria proposal decoders reject amplification and lifecycle drift", () => {
  const oversizedInbox = structuredClone(criteriaProposalList);
  oversizedInbox.proposals = Array.from({ length: 9 }, (_, index) => ({
    ...criteriaProposalSummary,
    proposal_id: `proposal-${index}`,
    proposal_seq: index + 1,
  }));
  expect(() => decodeWorkCriteriaProposalListV1(oversizedInbox)).toThrow(
    "ordered bounded pending inbox",
  );

  const acceptedWithoutResolution = structuredClone(criteriaProposalDetail) as {
    proposal: { status: string };
  };
  acceptedWithoutResolution.proposal.status = "accepted";
  expect(() => decodeWorkCriteriaProposalDetailV1(acceptedWithoutResolution)).toThrow(
    "status and resolution are incoherent",
  );

  const leakedSummary = structuredClone(criteriaProposalList) as {
    proposals: Array<Record<string, unknown>>;
  };
  leakedSummary.proposals[0].members = [];
  expect(() => decodeWorkCriteriaProposalListV1(leakedSummary)).toThrow(
    "unsupported field set",
  );
});

test("getWorkOverview sends the exact major and returns the strict projection", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, fixture));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({
    baseUrl: "https://astra.example",
    accessToken: "token",
  });

  const report = await client.getWorkOverview("work.a");
  expect(report.overview.work_id).toBe("work-1");
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work.a");
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
});

test("getWorkOverview rejects path-ambiguous identity before transport", async () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(client.getWorkOverview("work/a")).rejects.toThrow(
    "canonical Work resource identity",
  );
  expect(fetchMock).not.toHaveBeenCalled();
});

test("typed Work API errors survive the generic HTTP boundary", async () => {
  globalThis.fetch = vi.fn().mockResolvedValue(
    response(426, {
      code: "unsupported_client_version",
      category: "version",
      retryable: false,
      action_hints: ["upgrade_client"],
    }),
  );
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(client.getWorkOverview("work-1")).rejects.toMatchObject({
    status: 426,
    code: "unsupported_client_version",
    category: "version",
    retryable: false,
    actionHints: ["upgrade_client"],
  } satisfies Partial<AstraApiError>);
});

test("advanceWorkReadCursor sends one exact monotonic cursor and decodes its receipt", async () => {
  const receipt = {
    schema_version: 1,
    work_id: "work-1",
    through_event_seq: 42,
    receipt_revision: 3,
    receipt_hash: `sha256:${"a".repeat(64)}`,
    updated_at: "2026-08-01T01:02:03.123456Z",
  };
  const fetchMock = vi.fn().mockResolvedValue(response(200, receipt));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({
    baseUrl: "https://astra.example",
    accessToken: "token",
  });

  await expect(client.advanceWorkReadCursor("work-1", 42)).resolves.toEqual(receipt);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe("https://astra.example/v1/works/work-1/read-cursor");
  expect(init.method).toBe("PUT");
  expect(JSON.parse(String(init.body))).toEqual({ through_event_seq: 42 });
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
});

test("read-cursor boundary rejects ambiguous requests and incoherent receipts", async () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(client.advanceWorkReadCursor("work-1", 0)).rejects.toThrow(
    "positive safe integer",
  );
  expect(fetchMock).not.toHaveBeenCalled();

  const wrongWork = {
    schema_version: 1,
    work_id: "work-2",
    through_event_seq: 1,
    receipt_revision: 1,
    receipt_hash: `sha256:${"b".repeat(64)}`,
    updated_at: "2026-08-01T01:02:03Z",
  };
  globalThis.fetch = vi.fn().mockResolvedValue(response(200, wrongWork));
  await expect(client.advanceWorkReadCursor("work-1", 1)).rejects.toThrow(
    "different Work",
  );

  expect(() =>
    decodeWorkReadCursorReceiptV1({ ...wrongWork, legacy_session_id: "session-1" }),
  ).toThrow("unsupported field set");
});

test("listWorkEvents preserves the exact bounded cursor query", async () => {
  const page = {
    schema_version: 1,
    work_id: "work-1",
    requested_after_event_seq: 1,
    next_after_event_seq: 2,
    event_head: 2,
    retained_from_event_seq: 1,
    seen_through_event_seq: 1,
    coverage: "complete",
    has_more: false,
    events: [
      {
        event_seq: 2,
        branch_id: "branch-1",
        kind: "run_failed",
        work_revision: null,
        goal_revision: null,
        criterion_set_revision: null,
        branch_revision: null,
        graph_revision: 1,
        source_ref: "run:run-2",
        created_at: "2026-08-01T01:02:03Z",
      },
    ],
  };
  const fetchMock = vi.fn().mockResolvedValue(response(200, page));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(
    client.listWorkEvents("work-1", { afterEventSeq: 1, limit: 10 }),
  ).resolves.toEqual(page);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/events?after_event_seq=1&limit=10",
  );
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
});

test("event-page decoder rejects gaps and client rejects unbounded limits", async () => {
  const malformed = {
    schema_version: 1,
    work_id: "work-1",
    requested_after_event_seq: null,
    next_after_event_seq: 2,
    event_head: 2,
    retained_from_event_seq: 1,
    seen_through_event_seq: null,
    coverage: "complete",
    has_more: false,
    events: [
      {
        event_seq: 2,
        branch_id: null,
        kind: "work_created",
        work_revision: 1,
        goal_revision: 1,
        criterion_set_revision: 1,
        branch_revision: 1,
        graph_revision: 1,
        source_ref: "intent-1",
        created_at: "2026-08-01T01:02:03Z",
      },
    ],
  };
  expect(() => decodeWorkEventPageV1(malformed)).toThrow("sequence gap");

  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(client.listWorkEvents("work-1", { limit: 101 })).rejects.toThrow(
    "between 1 and 100",
  );
  expect(fetchMock).not.toHaveBeenCalled();
});

test("Work event reconciliation is contiguous, duplicate-safe, and fail-closed", () => {
  const event = (event_seq: number) => ({
    event_seq,
    branch_id: "branch-1",
    kind: "run_completed" as const,
    work_revision: null,
    goal_revision: null,
    criterion_set_revision: null,
    branch_revision: null,
    graph_revision: 1,
    source_ref: `run:run-${event_seq}`,
    created_at: "2026-08-01T01:02:03Z",
  });
  const page = (overrides: Partial<WorkEventPageV1> = {}): WorkEventPageV1 => ({
    schema_version: 1,
    work_id: "work-1",
    requested_after_event_seq: 2,
    next_after_event_seq: 4,
    event_head: 4,
    retained_from_event_seq: 1,
    seen_through_event_seq: 2,
    coverage: "complete",
    has_more: false,
    events: [event(3), event(4)],
    ...overrides,
  });
  const cursor = {
    work_id: "work-1",
    applied_through_event_seq: 2,
  };

  expect(reconcileWorkEventPageV1(cursor, page())).toEqual({
    kind: "applied",
    cursor: { work_id: "work-1", applied_through_event_seq: 4 },
    events: [event(3), event(4)],
    at_head: true,
  });
  expect(
    reconcileWorkEventPageV1(
      { work_id: "work-1", applied_through_event_seq: 4 },
      page(),
    ),
  ).toEqual({
    kind: "duplicate",
    cursor: { work_id: "work-1", applied_through_event_seq: 4 },
    at_head: true,
  });

  expect(
    reconcileWorkEventPageV1(
      cursor,
      page({
        requested_after_event_seq: 3,
        next_after_event_seq: 4,
        events: [event(4)],
      }),
    ),
  ).toEqual({
    kind: "gap",
    cursor,
    expected_after_event_seq: 2,
    observed_after_event_seq: 3,
  });

  expect(
    reconcileWorkEventPageV1(
      cursor,
      page({
        requested_after_event_seq: 2,
        retained_from_event_seq: 4,
        coverage: "expired",
        next_after_event_seq: 4,
        events: [event(4)],
      }),
    ),
  ).toEqual({
    kind: "expired",
    cursor,
    retained_from_event_seq: 4,
    event_head: 4,
  });

  expect(() =>
    reconcileWorkEventPageV1(
      { work_id: "work-2", applied_through_event_seq: 2 },
      page(),
    ),
  ).toThrow("different reconcile cursor");
});

const taskGraphPage = {
  schema_version: 2,
  scope: "declared_work",
  basis: {
    work_id: "work-1",
    work_revision: 1,
    goal_revision: 1,
    goal: "Ship a proven feature.",
    criteria_set_revision: 1,
    criteria_member_count: 0,
    criteria_manifest_hash: `sha256:${"a".repeat(64)}`,
    branch_id: "branch-1",
    branch_revision: 2,
    branch_goal_revision: 1,
    branch_criteria_set_revision: 1,
    branch_basis_graph_revision: 1,
    graph_revision: 2,
    graph_item_count: 2,
    graph_edge_count: 1,
    graph_manifest_hash: `sha256:${"b".repeat(64)}`,
  },
  cursor: { graph_revision: 2, item_offset: 0, dependency_offset: 0 },
  next_cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
  items: {
    offset: 0,
    limit: 1,
    total: 2,
    entries: [
      {
        item_id: "root",
        revision: 1,
        kind: "milestone",
        objective: "Ship a proven feature.",
        expected_result: "The result is reviewable.",
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
  dependencies: {
    offset: 0,
    limit: 1,
    total: 1,
    entries: [
      {
        predecessor_item_id: "root",
        successor_item_id: "implementation",
        kind: "dependency",
      },
    ],
  },
};

test("getWorkSessionBinding bootstraps exact public Work identity", async () => {
  const binding = {
    schema_version: 1,
    work_id: "work-1",
    branch_id: "branch-1",
    graph_revision: 2,
  } as const;
  const fetchMock = vi.fn().mockResolvedValue(response(200, binding));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  await expect(client.getWorkSessionBinding("session-1")).resolves.toEqual(binding);
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/session-bindings/session-1",
  );
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );

  await expect(client.getWorkSessionBinding("../session")).rejects.toThrow(
    "canonical Work binding identity",
  );
  expect(fetchMock).toHaveBeenCalledTimes(1);
});

test("Work session binding decoder rejects drift instead of guessing", () => {
  expect(() =>
    decodeWorkSessionBindingV1({
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      graph_revision: 0,
    }),
  ).toThrow("graph_revision");
  expect(() =>
    decodeWorkSessionBindingV1({
      schema_version: 1,
      work_id: "work-1",
      branch_id: "branch-1",
      graph_revision: 1,
      session_id: "must-not-leak",
    }),
  ).toThrow("field set");
});

test("getWorkTaskGraph reads one revision-pinned bounded branch page", async () => {
  const fetchMock = vi.fn().mockResolvedValue(response(200, taskGraphPage));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  const page = await client.getWorkTaskGraph("work-1", "branch-1", {
    itemLimit: 1,
    dependencyLimit: 1,
  });

  expect(page.next_cursor).toEqual({
    graph_revision: 2,
    item_offset: 1,
    dependency_offset: 1,
  });
  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/task-graph?item_limit=1&dependency_limit=1",
  );
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
  expect(JSON.stringify(page)).not.toContain("session");
});

test("shared Rust/TypeScript Task Graph v2 fixture preserves typed delivery", () => {
  const page = decodeWorkTaskGraphPageV2(taskGraphFixture);
  expect(page.items.entries[0]?.delivery).toEqual({
    status: "delivered",
    summary: "migration applied",
    blocker_kind: null,
    unavailable_capabilities: [],
  });
  expect(page.items.entries[0]?.execution.run).toMatchObject({
    run_id: "run-1",
    attempt_id: "attempt-1",
  });
});

test("getWorkTaskGraph forwards the complete pinned continuation cursor", async () => {
  const terminal = {
    ...structuredClone(taskGraphPage),
    cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
    next_cursor: null,
    items: {
      ...structuredClone(taskGraphPage.items),
      offset: 1,
      entries: [
        {
          item_id: "implementation",
          revision: 1,
          kind: "task",
          objective: "Implement the feature.",
          expected_result: "The implementation is reviewable.",
          declaration_state: "active",
          execution: {
            status: "completed",
            terminal: true,
            run: {
              run_id: "run-1",
              attempt_id: "run-1",
              graph_revision: 2,
              run_generation: 1,
              last_event_idx: 3,
              updated_at: "2026-08-01T12:00:00.000000Z",
            },
          },
          delivery: {
            status: "delivered",
            summary: "The implementation is ready for verification.",
            blocker_kind: null,
            unavailable_capabilities: [],
          },
          verification: {
            status: "evidence_available",
            latest_check: {
              check_run_id: "check-1",
              criterion: { criterion_id: "criterion-1", revision: 1 },
              criterion_set_revision: 2,
              graph_revision: 2,
              verifier_kind: "test",
              outcome: "passed",
              coverage: "complete",
              subject_revision: `sha256:${"c".repeat(64)}`,
              evidence_ref_count: 2,
              produced_at: "2026-08-01T12:00:00.000000Z",
              expires_at: null,
              freshness: "current",
            },
          },
        },
      ],
    },
    dependencies: {
      ...structuredClone(taskGraphPage.dependencies),
      offset: 1,
      entries: [],
    },
  };
  const fetchMock = vi.fn().mockResolvedValue(response(200, terminal));
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });

  const page = await client.getWorkTaskGraph("work-1", "branch-1", {
    cursor: { graph_revision: 2, item_offset: 1, dependency_offset: 1 },
  });

  expect(page.next_cursor).toBeNull();
  expect(fetchMock.mock.calls[0]?.[0]).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/task-graph?graph_revision=2&item_offset=1&dependency_offset=1",
  );
});

test("Task Graph boundary rejects torn cursors and malformed pages before use", async () => {
  const blocked = structuredClone(taskGraphPage);
  blocked.items.entries[0].delivery = {
    status: "blocked",
    summary: "The remote capability is unavailable.",
    blocker_kind: "capability_unavailable",
    unavailable_capabilities: ["web_fetch"],
  };
  expect(decodeWorkTaskGraphPageV2(blocked).items.entries[0]?.delivery).toEqual(
    blocked.items.entries[0].delivery,
  );

  const missingDelivery = structuredClone(taskGraphPage);
  delete (missingDelivery.items.entries[0] as { delivery?: unknown }).delivery;
  expect(() => decodeWorkTaskGraphPageV2(missingDelivery)).toThrow("field set");

  const wrongNext = structuredClone(taskGraphPage);
  wrongNext.next_cursor.item_offset = 2;
  expect(() => decodeWorkTaskGraphPageV2(wrongNext)).toThrow("next_cursor");

  const unknown = structuredClone(taskGraphPage) as Record<string, unknown>;
  unknown.execution_summary = { status: "completed" };
  expect(() => decodeWorkTaskGraphPageV2(unknown)).toThrow("field set");

  const oversized = structuredClone(taskGraphPage);
  oversized.items.limit = 9;
  expect(() => decodeWorkTaskGraphPageV2(oversized)).toThrow("between 0 and 8");

  const falseCompletion = structuredClone(taskGraphPage);
  (falseCompletion.items.entries[0] as { execution: unknown }).execution = {
    status: "completed",
    terminal: true,
    run: null,
  };
  expect(() => decodeWorkTaskGraphPageV2(falseCompletion)).toThrow("run presence");

  const mismatchedAttempt = structuredClone(taskGraphPage);
  (mismatchedAttempt.items.entries[0] as { execution: unknown }).execution = {
    status: "running",
    terminal: false,
    run: {
      run_id: "run-1",
      attempt_id: "attempt-2",
      graph_revision: 3,
      run_generation: 0,
      last_event_idx: -1,
      updated_at: "2026-08-01T12:00:00Z",
    },
  };
  expect(() => decodeWorkTaskGraphPageV2(mismatchedAttempt)).toThrow("root item attempt");

  const inferredVerification = structuredClone(taskGraphPage);
  (inferredVerification.items.entries[0] as { verification: unknown }).verification = {
    status: "evidence_available",
    latest_check: null,
  };
  expect(() => decodeWorkTaskGraphPageV2(inferredVerification)).toThrow(
    "status disagrees",
  );

  const incoherentDelivery = structuredClone(taskGraphPage);
  (incoherentDelivery.items.entries[0] as { delivery: unknown }).delivery = {
    status: "blocked",
    summary: "The required capability is unavailable.",
    blocker_kind: "capability_unavailable",
    unavailable_capabilities: [],
  };
  expect(() => decodeWorkTaskGraphPageV2(incoherentDelivery)).toThrow(
    "delivery facts are incoherent",
  );

  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  await expect(
    client.getWorkTaskGraph("work-1", "branch-1", {
      cursor: { graph_revision: 0, item_offset: 0, dependency_offset: 0 },
    }),
  ).rejects.toThrow("cursor");
  await expect(
    client.getWorkTaskGraph("work-1", "branch-1", { itemLimit: 9 }),
  ).rejects.toThrow("between 1 and 8");
  expect(fetchMock).not.toHaveBeenCalled();
});

test("continueWorkBranch sends one Work-only turn and validates its stream", async () => {
  const fetchMock = vi.fn().mockResolvedValue(
    streamResponse([
      {
        type: "work_turn_started",
        schema_version: 1,
        work_id: "work-1",
        branch_id: "branch-1",
        run_id: "run-1",
      },
      {
        type: "context_meta",
        system_prompt_tokens: 42,
        system_prompt_breakdown: { total_tokens: 42 },
      },
      {
        type: "work_task_graph_changed",
        schema_version: 1,
        graph_revision: 2,
        branch_revision: 3,
      },
      { type: "text_delta", content: "working" },
      { type: "run_finished", run_id: "run-1", status: "completed" },
    ]),
  );
  globalThis.fetch = fetchMock;
  const client = new AstraClient({
    baseUrl: "https://astra.example",
    accessToken: "token",
    headers: { "x-product": "web" },
  });
  const events: unknown[] = [];

  client.continueWorkBranch(
    "work-1",
    "branch-1",
    {
      requestId: "continue-1",
      attachmentId: "attachment-1",
      message: "Continue from the current Work facts.",
    },
    { onEvent: (event) => events.push(event) },
  );
  await vi.waitFor(() => expect(events).toHaveLength(5));

  const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
  expect(url).toBe(
    "https://astra.example/v1/works/work-1/branches/branch-1/turns",
  );
  expect(init.method).toBe("POST");
  expect(JSON.parse(String(init.body))).toEqual({
    request_id: "continue-1",
    attachment_id: "attachment-1",
    message: "Continue from the current Work facts.",
  });
  expect((init.headers as Record<string, string>)[ASTRA_WORK_API_MAJOR_HEADER]).toBe(
    ASTRA_WORK_API_MAJOR,
  );
  expect(String(init.body)).not.toContain("session");
  expect(JSON.stringify(events)).not.toContain("session_id");
});

test("continueWorkBranch preserves typed admission errors without retrying", async () => {
  const fetchMock = vi.fn().mockResolvedValue(
    response(409, {
      detail: "The submitted command conflicts with its durable identity.",
      code: "idempotency_mismatch",
      category: "conflict",
      retryable: false,
      action_hints: ["use_a_new_request_id"],
    }),
  );
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  const events: unknown[] = [];

  client.continueWorkBranch(
    "work-1",
    "branch-1",
    { requestId: "continue-1", attachmentId: "attachment-1", message: "Continue." },
    { onEvent: (event) => events.push(event) },
  );
  await vi.waitFor(() => expect(events).toHaveLength(1));

  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(events).toEqual([
    {
      type: "error",
      code: "idempotency_mismatch",
      error_code: "idempotency_mismatch",
      message: "The submitted command conflicts with its durable identity.",
      retryable: false,
      http_status: 409,
      category: "conflict",
      action_hints: ["use_a_new_request_id"],
    },
  ]);
});

test("Work turn boundary rejects ambiguous input and session-bearing events", () => {
  const fetchMock = vi.fn();
  globalThis.fetch = fetchMock;
  const client = new AstraClient({ baseUrl: "https://astra.example" });
  const callbacks = { onEvent: () => {} };

  expect(() =>
    client.continueWorkBranch(
      "work-1",
      "branch/1",
      { requestId: "continue-1", attachmentId: "attachment-1", message: "Continue." },
      callbacks,
    ),
  ).toThrow("branchId");
  expect(() =>
    client.continueWorkBranch(
      "work-1",
      "branch-1",
      { requestId: "continue-1", attachmentId: "attachment-1", message: " \n " },
      callbacks,
    ),
  ).toThrow("message");
  expect(() =>
    client.continueWorkBranch(
      "work-1",
      "branch-1",
      { requestId: "continue-1", attachmentId: "bad/attachment", message: "Continue." },
      callbacks,
    ),
  ).toThrow("attachmentId");
  expect(fetchMock).not.toHaveBeenCalled();

  expect(() =>
    decodeWorkTurnStreamEventV1({
      type: "run_started",
      run_id: "run-1",
      session_id: "internal-session",
    }),
  ).toThrow("internal session identity");
  expect(
    decodeWorkTurnStreamEventV1({
      type: "context_meta",
      system_prompt_tokens: 42,
      context_manifest_trace: { source: "canonical" },
    }),
  ).toMatchObject({ type: "context_meta", system_prompt_tokens: 42 });
  expect(() =>
    decodeWorkTurnStreamEventV1({
      type: "warning",
      details: { session_id: "internal-session" },
    }),
  ).toThrow("internal session identity");
  expect(() => decodeWorkTurnStreamEventV1({ type: "future_event" })).toThrow(
    "unsupported",
  );
});
