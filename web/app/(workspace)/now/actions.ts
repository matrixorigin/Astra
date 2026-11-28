"use server";

import type { WorkCatalogPageV1 } from "@astra/sdk";
import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  classifyWorkActionError,
  WORK_OBSERVATION_TIMEOUT_MS,
  type WorkActionError,
} from "@/lib/work-action-error";

export type RefreshNowWorkResult =
  | { ok: true; page: WorkCatalogPageV1 }
  | WorkActionError;

/** Refresh only the first bounded Work catalog page for the authenticated owner. */
export async function refreshNowWorkAction(): Promise<RefreshNowWorkResult> {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "refresh Now",
    });
    const page = await runtime.sdk.listWorks({
      limit: 20,
      signal: AbortSignal.timeout(WORK_OBSERVATION_TIMEOUT_MS),
    });
    return { ok: true, page };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}
