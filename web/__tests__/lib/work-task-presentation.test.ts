import type { WorkTaskGraphItemV2 } from "@astra/sdk";
import {
  isWorkTaskOpen,
  workTaskCounts,
  workTaskNeedsAttention,
  workTaskPresentation,
} from "@/lib/work-task-presentation";

function task(overrides: Partial<WorkTaskGraphItemV2> = {}): WorkTaskGraphItemV2 {
  return {
    item_id: "task-1",
    revision: 1,
    kind: "task",
    objective: "Deliver a verified change",
    expected_result: "The change is reviewable",
    declaration_state: "active",
    execution: { status: "not_started", terminal: false, run: null },
    delivery: {
      status: "unreported",
      summary: null,
      blocker_kind: null,
      unavailable_capabilities: [],
    },
    verification: { status: "unknown", latest_check: null },
    ...overrides,
  };
}

const terminalRun = {
  status: "completed",
  terminal: true,
  run: {
    run_id: "run-1",
    attempt_id: "run-1",
    graph_revision: 1,
    run_generation: 1,
    last_event_idx: 5,
    updated_at: "2026-08-04T00:00:00Z",
  },
} as const;

const delivered: WorkTaskGraphItemV2["delivery"] = {
  status: "delivered",
  summary: "The requested result was produced.",
  blocker_kind: null,
  unavailable_capabilities: [],
};

test("terminal execution, delivery, and verification remain independent", () => {
  const runOnly = task({ execution: terminalRun });
  expect(workTaskPresentation(runOnly).label).toBe("Result not reported");
  expect(isWorkTaskOpen(runOnly)).toBe(true);
  expect(workTaskNeedsAttention(runOnly)).toBe(true);

  const deliveredWithoutEvidence = task({ execution: terminalRun, delivery: delivered });
  expect(workTaskPresentation(deliveredWithoutEvidence).label).toBe("Needs verification");
  expect(isWorkTaskOpen(deliveredWithoutEvidence)).toBe(true);

  const checked = task({
    execution: terminalRun,
    delivery: delivered,
    verification: {
      status: "evidence_available",
      latest_check: {
        check_run_id: "check-1",
        criterion: { criterion_id: "criterion-1", revision: 1 },
        criterion_set_revision: 1,
        graph_revision: 1,
        verifier_kind: "test",
        outcome: "passed",
        coverage: "complete",
        subject_revision: `sha256:${"a".repeat(64)}`,
        evidence_ref_count: 1,
        produced_at: "2026-08-04T00:00:00Z",
        expires_at: null,
        freshness: "current",
      },
    },
  });
  expect(workTaskPresentation(checked)).toMatchObject({
    label: "Verified",
    verified: true,
    needsAttention: false,
  });
  expect(isWorkTaskOpen(checked)).toBe(false);
});

test("passed evidence is not verified without complete durable coverage", () => {
  const incomplete = task({
    execution: terminalRun,
    delivery: delivered,
    verification: {
      status: "evidence_available",
      latest_check: {
        check_run_id: "check-partial",
        criterion: { criterion_id: "criterion-1", revision: 1 },
        criterion_set_revision: 1,
        graph_revision: 1,
        verifier_kind: "test",
        outcome: "passed",
        coverage: "partial",
        subject_revision: `sha256:${"a".repeat(64)}`,
        evidence_ref_count: 1,
        produced_at: "2026-08-04T00:00:00Z",
        expires_at: null,
        freshness: "current",
      },
    },
  });

  expect(workTaskPresentation(incomplete)).toMatchObject({
    label: "Needs verification",
    verified: false,
    needsAttention: true,
  });
  expect(isWorkTaskOpen(incomplete)).toBe(true);
});

test("typed blockers and failed attempts stay visible as open attention", () => {
  const blocked = task({
    execution: terminalRun,
    delivery: {
      status: "blocked",
      summary: "A capability is unavailable.",
      blocker_kind: "capability_unavailable",
      unavailable_capabilities: ["web_fetch"],
    },
  });
  const failed = task({
    execution: { ...terminalRun, status: "failed" },
  });
  const replaced = task({ declaration_state: "superseded" });

  expect(workTaskPresentation(blocked).label).toBe("Blocked");
  expect(workTaskPresentation(failed).label).toBe("Failed");
  expect(workTaskCounts([blocked, failed, replaced])).toEqual({
    working: 0,
    planned: 0,
    attention: 2,
    verified: 0,
    open: 2,
  });
});
