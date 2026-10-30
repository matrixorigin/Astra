"use server";

import {
  type WorkBranchControlBasisV1,
  type WorkArchivedBranchCursorV1,
  type WorkArchivedBranchPageV1,
  type WorkBranchControlOperationV2,
  type WorkBranchActivityResponseV1,
  type WorkExecutionViewV1,
  type WorkExecutionTargetPageV1,
  type WorkExecutionSwitchOperationV1,
  type WorkBranchInteractionReceiptV1,
  type WorkBranchInteractionPageV1,
  type WorkBranchInteractionResponseInputV1,
  type WorkBranchCreationOperationV1,
  type WorkBranchDeletionOperationV1,
  type WorkBranchRetentionReceiptV1,
  type WorkBranchComparisonReportV2,
  type WorkDeliverySelectionReceiptV1,
  type WorkConversationHeadV1,
  type WorkCriteriaProposalDecisionInput,
  type WorkCriteriaProposalDetailV1,
  type WorkCriteriaProposalSummaryV1,
  type WorkReadCursorReceiptV1,
  type WorkTranscriptPageV1,
  type WorkEventPageV1,
  type WorkTaskGraphCursorV1,
  type WorkTaskGraphPageV2,
  decodeWorkBranchComparisonReportV2,
  decodeWorkConversationHeadV1,
} from "@astra/sdk";
import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  classifyWorkActionError,
  type WorkActionError,
} from "@/lib/work-action-error";
import {
  getWorkBranchPresentation,
  type WorkOverviewSnapshot,
} from "@/lib/work-overview";

export type LoadCriteriaProposalResult =
  | { ok: true; detail: WorkCriteriaProposalDetailV1 }
  | WorkActionError;

export type ResolveCriteriaProposalResult =
  | {
      ok: true;
      detail: WorkCriteriaProposalDetailV1;
      snapshot: WorkOverviewSnapshot;
    }
  | WorkActionError;

export type MarkWorkSeenResult =
  | { ok: true; receipt: WorkReadCursorReceiptV1 }
  | WorkActionError;

export type LoadWorkTranscriptResult =
  | { ok: true; page: WorkTranscriptPageV1 }
  | WorkActionError;

export type AcquireWorkBranchControlResult =
  | { ok: true; operation: WorkBranchControlOperationV2 }
  | WorkActionError;

export type ForceTakeoverWorkBranchResult =
  | { ok: true; operation: WorkBranchControlOperationV2 }
  | WorkActionError;

export type ObserveWorkBranchControlResult =
  | { ok: true; operation: WorkBranchControlOperationV2 }
  | WorkActionError;

export type AbortWorkBranchControlResult = { ok: true } | WorkActionError;

export type CreateWorkBranchResult =
  | { ok: true; operation: WorkBranchCreationOperationV1 }
  | WorkActionError;

export type ObserveWorkBranchCreationResult = CreateWorkBranchResult;
export type AbortWorkBranchCreationResult = { ok: true } | WorkActionError;

export type CompareWorkBranchesResult =
  | { ok: true; comparison: WorkBranchComparisonReportV2 }
  | WorkActionError;

export type SelectWorkDeliveryResult =
  | { ok: true; receipt: WorkDeliverySelectionReceiptV1 }
  | WorkActionError;

export type ChangeWorkBranchRetentionResult =
  | { ok: true; receipt: WorkBranchRetentionReceiptV1 }
  | WorkActionError;

export type LoadArchivedWorkBranchesResult =
  | { ok: true; page: WorkArchivedBranchPageV1 }
  | WorkActionError;

export type DeleteWorkBranchResult =
  | { ok: true; operation: WorkBranchDeletionOperationV1 }
  | WorkActionError;

export type ObserveWorkBranchDeletionResult = DeleteWorkBranchResult;

export type LoadWorkTaskGraphPageResult =
  | { ok: true; page: WorkTaskGraphPageV2 }
  | WorkActionError;
export type RefreshWorkTaskGraphResult = LoadWorkTaskGraphPageResult;

export type RefreshWorkBranchActivityResult =
  | { ok: true; activity: WorkBranchActivityResponseV1 }
  | WorkActionError;

/**
 * The lightweight live cursor used by the Work detail page.  A Work event
 * head is the shared cross-surface clock: polling it lets Web notice a turn
 * started in TUI (or on another Edge) without loading the whole transcript on
 * every tick.  The event page is read-only and never advances the user's
 * seen cursor.
 */
export type RefreshWorkEventsResult =
  | { ok: true; page: Pick<WorkEventPageV1, "work_id" | "event_head" | "events"> }
  | WorkActionError;

export type LoadWorkExecutionResult =
  | { ok: true; execution: WorkExecutionViewV1 }
  | WorkActionError;

export type LoadWorkExecutionTargetsResult =
  | { ok: true; page: WorkExecutionTargetPageV1 }
  | WorkActionError;

export type SwitchWorkExecutionResult =
  | { ok: true; operation: WorkExecutionSwitchOperationV1 }
  | WorkActionError;

export type ObserveWorkExecutionSwitchResult = SwitchWorkExecutionResult;
export type RetryWorkExecutionSwitchResult = SwitchWorkExecutionResult;

export type RespondWorkBranchInteractionResult =
  | { ok: true; receipt: WorkBranchInteractionReceiptV1 }
  | WorkActionError;

export type RefreshWorkBranchInteractionsResult =
  | { ok: true; page: WorkBranchInteractionPageV1 }
  | WorkActionError;

type RespondWorkBranchInteractionActionInput = WorkBranchInteractionResponseInputV1 & {
  workId: string;
  branchId: string;
};

function validRespondWorkBranchInteractionInput(
  input: RespondWorkBranchInteractionActionInput,
): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  if (
    Object.keys(object).sort().join("\0") !==
    "attachmentId\0branchId\0requestId\0response\0runId\0workId"
  ) {
    return false;
  }
  if (
    !canonicalWorkIdentity(object.workId) ||
    !canonicalWorkIdentity(object.branchId) ||
    !canonicalAttachmentIdentity(object.attachmentId) ||
    !canonicalInteractionIdentity(object.runId) ||
    !canonicalInteractionIdentity(object.requestId)
  ) {
    return false;
  }
  const response = object.response;
  if (!response || typeof response !== "object" || Array.isArray(response)) return false;
  const responseObject = response as Record<string, unknown>;
  if (responseObject.kind === "approval") {
    return (
      (responseObject.decision === "allow" || responseObject.decision === "deny") &&
      (responseObject.reason === undefined || typeof responseObject.reason === "string") &&
      Object.keys(responseObject)
        .sort()
        .join("\0")
        .match(/^decision\0kind(?:\0reason)?$/) !== null
    );
  }
  if (responseObject.kind === "user_prompt") {
    if (typeof responseObject.cancelled !== "boolean") return false;
    const keys = Object.keys(responseObject).sort().join("\0");
    return responseObject.cancelled
      ? keys === "cancelled\0kind"
      : keys === "answers\0cancelled\0kind" && responseObject.answers !== undefined;
  }
  return false;
}

/** Answer one Work-scoped pending interaction. This records a response for
 * the exact durable run and does not transfer branch controller ownership. */
export async function respondWorkBranchInteractionAction(
  input: RespondWorkBranchInteractionActionInput,
): Promise<RespondWorkBranchInteractionResult> {
  if (!validRespondWorkBranchInteractionInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_interaction_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "answer Work interaction",
    });
    const { workId, branchId, ...response } = input;
    return {
      ok: true,
      receipt: await runtime.sdk.respondWorkBranchInteraction(workId, branchId, response),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type RefreshWorkBranchInteractionsInput = { workId: string; branchId: string };

function validRefreshWorkBranchInteractionsInput(
  input: RefreshWorkBranchInteractionsInput,
): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "branchId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId)
  );
}

/** Read the current pending Work interaction without acquiring control. */
export async function refreshWorkBranchInteractionsAction(
  input: RefreshWorkBranchInteractionsInput,
): Promise<RefreshWorkBranchInteractionsResult> {
  if (!validRefreshWorkBranchInteractionsInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_interactions_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "refresh Work interactions",
    });
    return {
      ok: true,
      page: await runtime.sdk.getWorkBranchInteractions(input.workId, input.branchId),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type RefreshWorkBranchActivityInput = {
  workId: string;
  branchId: string;
};

function validWorkBranchActivityInput(
  input: RefreshWorkBranchActivityInput,
): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "branchId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId)
  );
}

/** Read the current branch's server-owned Run activity without acquiring control. */
export async function refreshWorkBranchActivityAction(
  input: RefreshWorkBranchActivityInput,
): Promise<RefreshWorkBranchActivityResult> {
  if (!validWorkBranchActivityInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_activity_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "refresh Work activity",
    });
    return {
      ok: true,
      activity: await runtime.sdk.getWorkBranchActivity(input.workId, input.branchId),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type RefreshWorkEventsInput = {
  workId: string;
  afterEventSeq: number;
};

function validWorkEventsInput(input: RefreshWorkEventsInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "afterEventSeq\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    Number.isSafeInteger(object.afterEventSeq) &&
    Number(object.afterEventSeq) >= 1
  );
}

/** Read only the Work event head so every surface can converge on live state. */
export async function refreshWorkEventsAction(
  input: RefreshWorkEventsInput,
): Promise<RefreshWorkEventsResult> {
  if (!validWorkEventsInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_events_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "refresh Work events",
    });
    const page = await runtime.sdk.listWorkEvents(input.workId, {
      afterEventSeq: input.afterEventSeq,
      limit: 1,
    });
    return {
      ok: true,
      page: {
        work_id: page.work_id,
        event_head: page.event_head,
        events: page.events,
      },
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type WorkExecutionIdentityInput = { workId: string; branchId: string };

function validWorkExecutionIdentity(input: WorkExecutionIdentityInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "branchId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId)
  );
}

/** Read the authoritative provider/generation without acquiring control. */
export async function loadWorkExecutionAction(
  input: WorkExecutionIdentityInput,
): Promise<LoadWorkExecutionResult> {
  if (!validWorkExecutionIdentity(input)) {
    return { ok: false, status: 400, code: "invalid_work_execution_query", retryable: false };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "read Work execution",
    });
    return {
      ok: true,
      execution: await runtime.sdk.getWorkBranchExecution(input.workId, input.branchId),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

/** Read the bounded owner-scoped Edge target directory on demand. */
export async function loadWorkExecutionTargetsAction(
  input: WorkExecutionIdentityInput,
): Promise<LoadWorkExecutionTargetsResult> {
  if (!validWorkExecutionIdentity(input)) {
    return { ok: false, status: 400, code: "invalid_work_execution_query", retryable: false };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "list Work execution targets",
    });
    return {
      ok: true,
      page: await runtime.sdk.listWorkBranchExecutionTargets(input.workId, input.branchId),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type SwitchWorkExecutionInput = WorkExecutionIdentityInput & {
  requestId: string;
  attachmentId: string;
  expectedGeneration: number;
  targetExecutorId: string;
};

function validWorkExecutionSwitchInput(input: SwitchWorkExecutionInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") ===
      "attachmentId\0branchId\0expectedGeneration\0requestId\0targetExecutorId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId) &&
    canonicalAttachmentIdentity(object.attachmentId) &&
    typeof object.requestId === "string" &&
    object.requestId.length >= 1 &&
    object.requestId.length <= 256 &&
    !/[\u0000-\u001f\u007f]/.test(object.requestId) &&
    Number.isSafeInteger(object.expectedGeneration) &&
    Number(object.expectedGeneration) >= 1 &&
    canonicalAttachmentIdentity(object.targetExecutorId)
  );
}

/** Begin an explicit Edge to Edge Work execution handoff. */
export async function switchWorkExecutionAction(
  input: SwitchWorkExecutionInput,
): Promise<SwitchWorkExecutionResult> {
  if (!validWorkExecutionSwitchInput(input)) {
    return { ok: false, status: 400, code: "invalid_work_execution_switch", retryable: false };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "move Work execution",
    });
    return {
      ok: true,
      operation: await runtime.sdk.switchWorkBranchExecution(input.workId, input.branchId, {
        requestId: input.requestId,
        attachmentId: input.attachmentId,
        expectedGeneration: input.expectedGeneration,
        target: { kind: "edge", executorId: input.targetExecutorId },
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type WorkExecutionSwitchIdentityInput = WorkExecutionIdentityInput & {
  operationId: string;
};

function validWorkExecutionSwitchIdentity(input: WorkExecutionSwitchIdentityInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "branchId\0operationId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId) &&
    canonicalAttachmentIdentity(object.operationId)
  );
}

/** Observe a durable handoff without repeating workspace attestation. */
export async function observeWorkExecutionSwitchAction(
  input: WorkExecutionSwitchIdentityInput,
): Promise<ObserveWorkExecutionSwitchResult> {
  if (!validWorkExecutionSwitchIdentity(input)) {
    return { ok: false, status: 400, code: "invalid_work_execution_switch", retryable: false };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "observe Work execution move",
    });
    return {
      ok: true,
      operation: await runtime.sdk.getWorkBranchExecutionSwitch(
        input.workId,
        input.branchId,
        input.operationId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type RetryWorkExecutionSwitchInput = WorkExecutionSwitchIdentityInput & {
  attachmentId: string;
};

function validWorkExecutionRetryInput(input: RetryWorkExecutionSwitchInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") ===
      "attachmentId\0branchId\0operationId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId) &&
    canonicalAttachmentIdentity(object.operationId) &&
    canonicalAttachmentIdentity(object.attachmentId)
  );
}

/** Retry a failed handoff with the current controller attachment. */
export async function retryWorkExecutionSwitchAction(
  input: RetryWorkExecutionSwitchInput,
): Promise<RetryWorkExecutionSwitchResult> {
  if (!validWorkExecutionRetryInput(input)) {
    return { ok: false, status: 400, code: "invalid_work_execution_switch", retryable: false };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "retry Work execution move",
    });
    return {
      ok: true,
      operation: await runtime.sdk.retryWorkBranchExecutionSwitch(
        input.workId,
        input.branchId,
        input.operationId,
        input.attachmentId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type RefreshWorkTaskGraphInput = {
  workId: string;
  branchId: string;
};

function validTaskGraphIdentity(input: RefreshWorkTaskGraphInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  return (
    Object.keys(object).sort().join("\0") === "branchId\0workId" &&
    canonicalWorkIdentity(object.workId) &&
    canonicalWorkIdentity(object.branchId)
  );
}

/** Read only the bounded Task Graph head used for active-run visualization. */
export async function refreshWorkTaskGraphAction(
  input: RefreshWorkTaskGraphInput,
): Promise<RefreshWorkTaskGraphResult> {
  if (!validTaskGraphIdentity(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_task_graph_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "refresh active Work plan",
    });
    return {
      ok: true,
      page: await runtime.sdk.getWorkTaskGraph(input.workId, input.branchId, {
        itemLimit: 8,
        dependencyLimit: 128,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type LoadWorkTaskGraphPageInput = {
  workId: string;
  branchId: string;
  cursor: WorkTaskGraphCursorV1;
};

function validTaskGraphPageInput(input: LoadWorkTaskGraphPageInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  if (
    Object.keys(object).sort().join("\0") !== "branchId\0cursor\0workId" ||
    !canonicalWorkIdentity(object.workId) ||
    !canonicalWorkIdentity(object.branchId) ||
    !object.cursor ||
    typeof object.cursor !== "object" ||
    Array.isArray(object.cursor)
  ) {
    return false;
  }
  const cursor = object.cursor as Record<string, unknown>;
  return (
    Object.keys(cursor).sort().join("\0") ===
      "dependency_offset\0graph_revision\0item_offset" &&
    Number.isSafeInteger(cursor.graph_revision) &&
    Number(cursor.graph_revision) >= 1 &&
    Number.isSafeInteger(cursor.item_offset) &&
    Number(cursor.item_offset) >= 0 &&
    Number(cursor.item_offset) <= 256 &&
    Number.isSafeInteger(cursor.dependency_offset) &&
    Number(cursor.dependency_offset) >= 0 &&
    Number(cursor.dependency_offset) <= 1024
  );
}

/** Continue one immutable Task Graph read; never infer or silently rebase. */
export async function loadWorkTaskGraphPageAction(
  input: LoadWorkTaskGraphPageInput,
): Promise<LoadWorkTaskGraphPageResult> {
  if (!validTaskGraphPageInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_task_graph_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load more Work plan items",
    });
    return {
      ok: true,
      page: await runtime.sdk.getWorkTaskGraph(input.workId, input.branchId, {
        cursor: input.cursor,
        itemLimit: 8,
        dependencyLimit: 128,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

type ChangeWorkBranchRetentionInput = {
  workId: string;
  branchId: string;
  requestId: string;
  expectedWorkRevision: number;
  expectedBranchRevision: number;
  kind: "archive" | "restore";
};

type LoadArchivedWorkBranchesInput = {
  workId: string;
  before: WorkArchivedBranchCursorV1;
};

type DeleteWorkBranchInput = {
  workId: string;
  branchId: string;
  requestId: string;
  expectedWorkRevision: number;
  expectedBranchRevision: number;
};

type WorkBranchDeletionOperationInput = {
  workId: string;
  branchId: string;
  operationId: string;
};

type LoadCriteriaProposalInput = {
  workId: string;
  branchId: string;
  proposalId: string;
};

type ResolveCriteriaProposalInput = {
  workId: string;
  branchId: string;
  proposal: WorkCriteriaProposalSummaryV1;
  decision: WorkCriteriaProposalDecisionInput["decision"];
  requestId: string;
};

type MarkWorkSeenInput = {
  workId: string;
  throughEventSeq: number;
};

type LoadWorkTranscriptInput = {
  workId: string;
  branchId: string;
  beforeItemSeq: number;
};

type AcquireWorkBranchControlInput = {
  workId: string;
  branchId: string;
  attachmentId: string;
  requestId: string;
  expectedBranchRevision: number;
  expectedControlBasis: WorkBranchControlBasisV1;
};

type ForceTakeoverWorkBranchInput = AcquireWorkBranchControlInput & {
  password?: string;
  memoriaProof?: string;
};

type WorkBranchControlOperationInput = {
  workId: string;
  branchId: string;
  operationId: string;
};

type CreateWorkBranchInput = {
  workId: string;
  originBranchId: string;
  requestId: string;
  expectedBranchRevision: number;
  committedCursor: WorkConversationHeadV1;
};

type WorkBranchCreationOperationInput = {
  workId: string;
  originBranchId: string;
  operationId: string;
};

type CompareWorkBranchesInput = {
  workId: string;
  leftBranchId: string;
  rightBranchId: string;
};

type SelectWorkDeliveryInput = {
  workId: string;
  requestId: string;
  comparison: WorkBranchComparisonReportV2;
};

function parseDeliverySelectionInput(input: unknown): SelectWorkDeliveryInput | null {
  if (!input || typeof input !== "object" || Array.isArray(input)) return null;
  const value = input as Record<string, unknown>;
  if (
    Object.keys(value).sort().join("\0") !== "comparison\0requestId\0workId" ||
    !canonicalWorkIdentity(value.workId) ||
    typeof value.requestId !== "string" ||
    value.requestId.length < 1 ||
    value.requestId.length > 256 ||
    /[\u0000-\u001f\u007f]/.test(value.requestId)
  ) {
    return null;
  }
  try {
    const comparison = decodeWorkBranchComparisonReportV2(value.comparison);
    if (
      comparison.work_id !== value.workId ||
      !comparison.directly_comparable ||
      comparison.left.is_delivery ||
      !comparison.right.is_delivery
    ) {
      return null;
    }
    return {
      workId: value.workId,
      requestId: value.requestId,
      comparison,
    };
  } catch {
    return null;
  }
}

function validBranchComparisonInput(
  input: unknown,
): input is CompareWorkBranchesInput {
  if (!input || typeof input !== "object" || Array.isArray(input)) return false;
  const value = input as Record<string, unknown>;
  return (
    Object.keys(value).sort().join("\0") ===
      "leftBranchId\0rightBranchId\0workId" &&
    canonicalWorkIdentity(value.workId) &&
    canonicalWorkIdentity(value.leftBranchId) &&
    canonicalWorkIdentity(value.rightBranchId) &&
    value.leftBranchId !== value.rightBranchId
  );
}

function validBranchCreationOperationInput(
  input: unknown,
): input is WorkBranchCreationOperationInput {
  if (!input || typeof input !== "object" || Array.isArray(input)) return false;
  const value = input as Record<string, unknown>;
  return (
    Object.keys(value).sort().join("\0") ===
      "operationId\0originBranchId\0workId" &&
    canonicalWorkIdentity(value.workId) &&
    canonicalWorkIdentity(value.originBranchId) &&
    canonicalWorkIdentity(value.operationId)
  );
}

function validBranchCreationInput(input: unknown): input is CreateWorkBranchInput {
  if (!input || typeof input !== "object" || Array.isArray(input)) return false;
  const value = input as Record<string, unknown>;
  if (
    Object.keys(value).sort().join("\0") !==
      "committedCursor\0expectedBranchRevision\0originBranchId\0requestId\0workId" ||
    !canonicalWorkIdentity(value.workId) ||
    !canonicalWorkIdentity(value.originBranchId) ||
    typeof value.requestId !== "string" ||
    value.requestId.length < 1 ||
    value.requestId.length > 256 ||
    /[\u0000-\u001f\u007f]/.test(value.requestId) ||
    !Number.isSafeInteger(value.expectedBranchRevision) ||
    Number(value.expectedBranchRevision) < 1
  ) {
    return false;
  }
  try {
    decodeWorkConversationHeadV1(value.committedCursor);
    return true;
  } catch {
    return false;
  }
}

function validControlOperationInput(
  input: unknown,
): input is WorkBranchControlOperationInput {
  if (!input || typeof input !== "object" || Array.isArray(input)) return false;
  const value = input as Record<string, unknown>;
  return (
    Object.keys(value).sort().join("\0") === "branchId\0operationId\0workId" &&
    canonicalWorkIdentity(value.workId) &&
    canonicalWorkIdentity(value.branchId) &&
    canonicalWorkIdentity(value.operationId)
  );
}

function validControlInput(
  input: unknown,
  expectedFields: readonly string[],
): input is AcquireWorkBranchControlInput {
  if (!input || typeof input !== "object" || Array.isArray(input)) return false;
  const value = input as Record<string, unknown>;
  const fields = Object.keys(value).sort();
  return (
    fields.length === expectedFields.length &&
    fields.every((field, index) => field === expectedFields[index]) &&
    canonicalWorkIdentity(value.workId) &&
    canonicalWorkIdentity(value.branchId) &&
    canonicalAttachmentIdentity(value.attachmentId) &&
    typeof value.requestId === "string" &&
    value.requestId.length >= 1 &&
    value.requestId.length <= 256 &&
    !/[\u0000-\u001f\u007f]/.test(value.requestId) &&
    Number.isSafeInteger(value.expectedBranchRevision) &&
    Number(value.expectedBranchRevision) >= 1 &&
    validControlBasis(value.expectedControlBasis)
  );
}

function canonicalWorkIdentity(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length >= 1 &&
    value.length <= 64 &&
    value !== "." &&
    value !== ".." &&
    /^[A-Za-z0-9._-]+$/.test(value)
  );
}

function canonicalAttachmentIdentity(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length >= 1 &&
    value.length <= 128 &&
    value !== "." &&
    value !== ".." &&
    /^[A-Za-z0-9._:-]+$/.test(value)
  );
}

function canonicalInteractionIdentity(value: unknown): value is string {
  return (
    typeof value === "string" &&
    value.length >= 1 &&
    value.length <= 256 &&
    !/[\u0000-\u001f\u007f-\u009f]/u.test(value)
  );
}

function validControlBasis(value: unknown): value is WorkBranchControlBasisV1 {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  const fields = Object.keys(object).sort();
  return (
    fields.length === 2 &&
    fields[0] === "canonical_root_hash" &&
    fields[1] === "writer_epoch" &&
    Number.isSafeInteger(object.writer_epoch) &&
    Number(object.writer_epoch) >= 0 &&
    (object.canonical_root_hash === null ||
      (typeof object.canonical_root_hash === "string" &&
        /^[0-9a-f]{64}$/.test(object.canonical_root_hash)))
  );
}

export async function acquireWorkBranchControlAction(
  input: AcquireWorkBranchControlInput,
): Promise<AcquireWorkBranchControlResult> {
  if (!validControlInput(input, [
    "attachmentId",
    "branchId",
    "expectedBranchRevision",
    "expectedControlBasis",
    "requestId",
    "workId",
  ])) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_control_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "continue Work here",
    });
    return {
      ok: true,
      operation: await runtime.sdk.controlWorkBranch(input.workId, input.branchId, {
        requestId: input.requestId,
        expectedBranchRevision: input.expectedBranchRevision,
        expectedControlBasis: input.expectedControlBasis,
        command: {
          kind: "acquire_branch_control",
          attachmentId: input.attachmentId,
        },
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function getWorkReauthenticationOptionsAction() {
  const runtime = await requireRuntimeClient({ auth: "required", operation: "verify identity before moving Work" });
  return runtime.sdk.getReauthenticationOptions();
}

export async function forceTakeoverWorkBranchAction(
  input: ForceTakeoverWorkBranchInput,
): Promise<ForceTakeoverWorkBranchResult> {
  if (
    !validControlInput(input, [
      "attachmentId",
      "branchId",
      "expectedBranchRevision",
      "expectedControlBasis",
      input && typeof input === "object" && "memoriaProof" in input ? "memoriaProof" : "password",
      "requestId",
      "workId",
    ]) ||
    ("memoriaProof" in input
      ? typeof input.memoriaProof !== "string" || !/^msu_[a-f0-9]{64}$/.test(input.memoriaProof)
      : typeof input.password !== "string" || input.password.length < 1 || input.password.length > 4096)
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_control_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "continue Work on this device",
    });
    const authorization = await runtime.sdk.reauthenticate(
      "memoriaProof" in input ? { memoriaProof: input.memoriaProof! } : input.password!,
      "session_forced_takeover",
    );
    return {
      ok: true,
      operation: await runtime.sdk.controlWorkBranch(input.workId, input.branchId, {
        requestId: input.requestId,
        expectedBranchRevision: input.expectedBranchRevision,
        expectedControlBasis: input.expectedControlBasis,
        command: {
          kind: "force_takeover",
          attachmentId: input.attachmentId,
          reauthenticationProof: authorization.proof,
        },
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function observeWorkBranchControlAction(
  input: WorkBranchControlOperationInput,
): Promise<ObserveWorkBranchControlResult> {
  if (!validControlOperationInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_control_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "observe Work move",
    });
    return {
      ok: true,
      operation: await runtime.sdk.getWorkBranchControlOperation(
        input.workId,
        input.branchId,
        input.operationId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function abortWorkBranchControlAction(
  input: WorkBranchControlOperationInput,
): Promise<AbortWorkBranchControlResult> {
  if (!validControlOperationInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_control_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "stop moving Work",
    });
    await runtime.sdk.abortWorkBranchControlOperation(
      input.workId,
      input.branchId,
      input.operationId,
    );
    return { ok: true };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function createWorkBranchAction(
  input: CreateWorkBranchInput,
): Promise<CreateWorkBranchResult> {
  if (!validBranchCreationInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_creation_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "try another Work approach",
    });
    return {
      ok: true,
      operation: await runtime.sdk.forkWorkBranch(input.workId, input.originBranchId, {
        requestId: input.requestId,
        expectedBranchRevision: input.expectedBranchRevision,
        committedCursor: input.committedCursor,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function observeWorkBranchCreationAction(
  input: WorkBranchCreationOperationInput,
): Promise<ObserveWorkBranchCreationResult> {
  if (!validBranchCreationOperationInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_creation_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "observe Work alternative creation",
    });
    return {
      ok: true,
      operation: await runtime.sdk.getWorkBranchForkOperation(
        input.workId,
        input.originBranchId,
        input.operationId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function abortWorkBranchCreationAction(
  input: WorkBranchCreationOperationInput,
): Promise<AbortWorkBranchCreationResult> {
  if (!validBranchCreationOperationInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_creation_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "stop creating Work alternative",
    });
    await runtime.sdk.abortWorkBranchForkOperation(
      input.workId,
      input.originBranchId,
      input.operationId,
    );
    return { ok: true };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function compareWorkBranchesAction(
  input: CompareWorkBranchesInput,
): Promise<CompareWorkBranchesResult> {
  if (!validBranchComparisonInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_comparison_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "compare Work approaches",
    });
    return {
      ok: true,
      comparison: await runtime.sdk.compareWorkBranches(
        input.workId,
        input.leftBranchId,
        input.rightBranchId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function selectWorkDeliveryAction(
  input: SelectWorkDeliveryInput,
): Promise<SelectWorkDeliveryResult> {
  const parsed = parseDeliverySelectionInput(input);
  if (!parsed) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_action_request",
      retryable: false,
    };
  }
  const candidate = parsed.comparison.left;
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "use Work result",
    });
    return {
      ok: true,
      receipt: await runtime.sdk.selectWorkDeliveryBranch(parsed.workId, {
        requestId: parsed.requestId,
        branchId: candidate.branch_id,
        expectedWorkRevision: parsed.comparison.work_revision,
        expectedBranchRevision: candidate.branch_revision,
        expectedGoalRevision: candidate.goal_revision_ref,
        expectedCriteriaSetRevision: candidate.criteria.revision,
        expectedGraphRevision: candidate.graph.current_revision,
        expectedSubject:
          candidate.subject === null
            ? null
            : {
                graphRevision: candidate.subject.graph_revision,
                subjectRef: candidate.subject.subject_ref,
                subjectRevision: candidate.subject.subject_revision,
              },
        expectedEvidenceManifestHash:
          parsed.comparison.left_evidence.manifest_hash,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function changeWorkBranchRetentionAction(
  input: ChangeWorkBranchRetentionInput,
): Promise<ChangeWorkBranchRetentionResult> {
  const value = input as unknown;
  const object =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  if (
    !object ||
    Object.keys(object).sort().join(",") !==
      "branchId,expectedBranchRevision,expectedWorkRevision,kind,requestId,workId" ||
    !canonicalWorkIdentity(object.workId) ||
    !canonicalWorkIdentity(object.branchId) ||
    typeof object.requestId !== "string" ||
    object.requestId.length === 0 ||
    new TextEncoder().encode(object.requestId).length > 256 ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(object.requestId) ||
    !Number.isSafeInteger(object.expectedWorkRevision) ||
    Number(object.expectedWorkRevision) < 1 ||
    !Number.isSafeInteger(object.expectedBranchRevision) ||
    Number(object.expectedBranchRevision) < 1 ||
    (object.kind !== "archive" && object.kind !== "restore")
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_action_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: `${input.kind} Work approach`,
    });
    const request = {
      requestId: input.requestId,
      expectedWorkRevision: input.expectedWorkRevision,
      expectedBranchRevision: input.expectedBranchRevision,
    };
    return {
      ok: true,
      receipt:
        input.kind === "archive"
          ? await runtime.sdk.archiveWorkBranch(input.workId, input.branchId, request)
          : await runtime.sdk.restoreWorkBranch(input.workId, input.branchId, request),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function loadArchivedWorkBranchesAction(
  input: LoadArchivedWorkBranchesInput,
): Promise<LoadArchivedWorkBranchesResult> {
  const value = input as unknown;
  const object =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  const before = object?.before;
  const cursor =
    before && typeof before === "object" && !Array.isArray(before)
      ? (before as Record<string, unknown>)
      : null;
  if (
    !object ||
    Object.keys(object).sort().join(",") !== "before,workId" ||
    !canonicalWorkIdentity(object.workId) ||
    !cursor ||
    Object.keys(cursor).sort().join(",") !== "archived_at,branch_id" ||
    !canonicalWorkIdentity(cursor.branch_id) ||
    typeof cursor.archived_at !== "string" ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z$/u.test(
      cursor.archived_at,
    ) ||
    !Number.isFinite(Date.parse(cursor.archived_at))
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_archived_branch_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load archived Work approaches",
    });
    return {
      ok: true,
      page: await runtime.sdk.listArchivedWorkBranches(input.workId, {
        before: input.before,
        limit: 20,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function deleteWorkBranchAction(
  input: DeleteWorkBranchInput,
): Promise<DeleteWorkBranchResult> {
  const value = input as unknown;
  const object =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  if (
    !object ||
    Object.keys(object).sort().join(",") !==
      "branchId,expectedBranchRevision,expectedWorkRevision,requestId,workId" ||
    !canonicalWorkIdentity(object.workId) ||
    !canonicalWorkIdentity(object.branchId) ||
    typeof object.requestId !== "string" ||
    object.requestId.length === 0 ||
    new TextEncoder().encode(object.requestId).length > 256 ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(object.requestId) ||
    !Number.isSafeInteger(object.expectedWorkRevision) ||
    Number(object.expectedWorkRevision) < 1 ||
    !Number.isSafeInteger(object.expectedBranchRevision) ||
    Number(object.expectedBranchRevision) < 1
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_deletion_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "delete archived Work approach",
    });
    return {
      ok: true,
      operation: await runtime.sdk.deleteWorkBranch(input.workId, input.branchId, {
        requestId: input.requestId,
        expectedWorkRevision: input.expectedWorkRevision,
        expectedBranchRevision: input.expectedBranchRevision,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function observeWorkBranchDeletionAction(
  input: WorkBranchDeletionOperationInput,
): Promise<ObserveWorkBranchDeletionResult> {
  const value = input as unknown;
  const object =
    value && typeof value === "object" && !Array.isArray(value)
      ? (value as Record<string, unknown>)
      : null;
  if (
    !object ||
    Object.keys(object).sort().join(",") !== "branchId,operationId,workId" ||
    !canonicalWorkIdentity(object.workId) ||
    !canonicalWorkIdentity(object.branchId) ||
    !canonicalWorkIdentity(object.operationId)
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_branch_deletion_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "observe Work approach deletion",
    });
    return {
      ok: true,
      operation: await runtime.sdk.getWorkBranchDeletionOperation(
        input.workId,
        input.branchId,
        input.operationId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function loadWorkTranscriptPageAction(
  input: LoadWorkTranscriptInput,
): Promise<LoadWorkTranscriptResult> {
  const fields = input && typeof input === "object" && !Array.isArray(input)
    ? Object.keys(input).sort()
    : [];
  if (
    !input ||
    typeof input !== "object" ||
    fields.length !== 3 ||
    fields[0] !== "beforeItemSeq" ||
    fields[1] !== "branchId" ||
    fields[2] !== "workId" ||
    !canonicalWorkIdentity(input.workId) ||
    !canonicalWorkIdentity(input.branchId) ||
    !Number.isSafeInteger(input.beforeItemSeq) ||
    input.beforeItemSeq < 1
  ) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_transcript_query",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load earlier Work conversation",
    });
    return {
      ok: true,
      page: await runtime.sdk.getWorkBranchTranscript(input.workId, input.branchId, {
        beforeItemSeq: input.beforeItemSeq,
        limit: 50,
      }),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

function validMarkWorkSeenInput(input: MarkWorkSeenInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  const fields = Object.keys(object).sort();
  return (
    fields.length === 2 &&
    fields[0] === "throughEventSeq" &&
    fields[1] === "workId" &&
    canonicalWorkIdentity(object.workId) &&
    typeof object.throughEventSeq === "number" &&
    Number.isSafeInteger(object.throughEventSeq) &&
    object.throughEventSeq >= 1
  );
}

export async function markWorkSeenAction(
  input: MarkWorkSeenInput,
): Promise<MarkWorkSeenResult> {
  if (!validMarkWorkSeenInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_read_cursor_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "mark Work updates seen",
    });
    return {
      ok: true,
      receipt: await runtime.sdk.advanceWorkReadCursor(
        input.workId,
        input.throughEventSeq,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function loadCriteriaProposalAction(
  input: LoadCriteriaProposalInput,
): Promise<LoadCriteriaProposalResult> {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "review suggested Done-when criteria",
    });
    return {
      ok: true,
      detail: await runtime.sdk.getWorkCriteriaProposal(
        input.workId,
        input.branchId,
        input.proposalId,
      ),
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}

export async function resolveCriteriaProposalAction(
  input: ResolveCriteriaProposalInput,
): Promise<ResolveCriteriaProposalResult> {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: `${input.decision} suggested Done-when criteria`,
    });
    const detail = await runtime.sdk.resolveWorkCriteriaProposal(
      input.workId,
      input.branchId,
      input.proposal,
      { requestId: input.requestId, decision: input.decision },
    );
    const { snapshot } = await getWorkBranchPresentation(
      runtime.sdk,
      input.workId,
      input.branchId,
    );
    return { ok: true, detail, snapshot };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}
