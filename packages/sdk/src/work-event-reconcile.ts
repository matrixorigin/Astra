import type { WorkEventPageV1, WorkEventRecordV1 } from "./types";

/** The last contiguous semantic Work event applied by a consumer. */
export type WorkEventReconcileCursorV1 = {
  work_id: string;
  applied_through_event_seq: number | null;
};

export type WorkEventReconcileResultV1 =
  | {
      kind: "applied";
      cursor: WorkEventReconcileCursorV1;
      events: WorkEventRecordV1[];
      at_head: boolean;
    }
  | {
      kind: "duplicate";
      cursor: WorkEventReconcileCursorV1;
      at_head: boolean;
    }
  | {
      kind: "gap";
      cursor: WorkEventReconcileCursorV1;
      expected_after_event_seq: number | null;
      observed_after_event_seq: number | null;
    }
  | {
      kind: "expired";
      cursor: WorkEventReconcileCursorV1;
      retained_from_event_seq: number;
      event_head: number;
    };

/**
 * Reconcile one already-decoded Work event page against a consumer's durable
 * contiguous cursor. This is intentionally a pure sequence protocol: it does
 * not infer intent from event kinds and never skips an unobserved event.
 */
export function reconcileWorkEventPageV1(
  cursor: WorkEventReconcileCursorV1,
  page: WorkEventPageV1,
): WorkEventReconcileResultV1 {
  if (cursor.work_id !== page.work_id) {
    throw new TypeError("Work event page belongs to a different reconcile cursor");
  }
  const appliedThrough = cursor.applied_through_event_seq ?? 0;
  if (
    !Number.isSafeInteger(appliedThrough) ||
    appliedThrough < 0 ||
    (cursor.applied_through_event_seq !== null && appliedThrough === 0)
  ) {
    throw new TypeError(
      "applied_through_event_seq must be null or a positive safe integer",
    );
  }

  const retentionPredecessor = page.retained_from_event_seq - 1;
  if (appliedThrough < retentionPredecessor) {
    return {
      kind: "expired",
      cursor,
      retained_from_event_seq: page.retained_from_event_seq,
      event_head: page.event_head,
    };
  }

  const requestedAfter = page.requested_after_event_seq ?? 0;
  if (requestedAfter > appliedThrough) {
    return {
      kind: "gap",
      cursor,
      expected_after_event_seq: cursor.applied_through_event_seq,
      observed_after_event_seq: page.requested_after_event_seq,
    };
  }

  // A late response from an earlier poll is harmless. Never move the cursor
  // backwards or replace a newer head with this page.
  if (page.event_head <= appliedThrough) {
    return {
      kind: "duplicate",
      cursor,
      at_head: page.event_head === appliedThrough,
    };
  }

  const events = page.events.filter((event) => event.event_seq > appliedThrough);
  if (events.length === 0) {
    return {
      kind: "duplicate",
      cursor,
      at_head: false,
    };
  }
  if (events[0].event_seq !== appliedThrough + 1) {
    return {
      kind: "gap",
      cursor,
      expected_after_event_seq: cursor.applied_through_event_seq,
      observed_after_event_seq: page.requested_after_event_seq,
    };
  }

  const nextCursor: WorkEventReconcileCursorV1 = {
    work_id: cursor.work_id,
    applied_through_event_seq: events[events.length - 1].event_seq,
  };
  return {
    kind: "applied",
    cursor: nextCursor,
    events,
    at_head: nextCursor.applied_through_event_seq === page.event_head,
  };
}
