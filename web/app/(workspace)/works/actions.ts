"use server";

import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  classifyWorkActionError,
  type WorkActionError,
} from "@/lib/work-action-error";

type StartWorkInput = { requestId: string; goal: string };
const encoder = new TextEncoder();

export type StartWorkResult =
  | { ok: true; workId: string; branchId: string }
  | WorkActionError;

function validStartWorkInput(input: StartWorkInput): boolean {
  const value = input as unknown;
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const object = value as Record<string, unknown>;
  const fields = Object.keys(object).sort();
  return (
    fields.length === 2 &&
    fields[0] === "goal" &&
    fields[1] === "requestId" &&
    typeof object.requestId === "string" &&
    encoder.encode(object.requestId).length >= 1 &&
    encoder.encode(object.requestId).length <= 256 &&
    !/\p{Cc}/u.test(object.requestId) &&
    typeof object.goal === "string" &&
    object.goal.trim().length > 0 &&
    encoder.encode(object.goal).length <= 8 * 1024
  );
}

export async function startWorkAction(
  input: StartWorkInput,
): Promise<StartWorkResult> {
  if (!validStartWorkInput(input)) {
    return {
      ok: false,
      status: 400,
      code: "invalid_work_create_request",
      retryable: false,
    };
  }
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "start Work",
    });
    const report = await runtime.sdk.createWork({
      requestId: input.requestId,
      goal: input.goal,
      criteria: [],
    });
    return {
      ok: true,
      workId: report.overview.work_id,
      branchId: report.overview.delivery_branch.branch_id,
    };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}
