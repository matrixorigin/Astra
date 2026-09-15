"use server";

import type { WorkCatalogPageV1 } from "@astra/sdk";
import { requireRuntimeClient } from "@/lib/runtime-client";
import {
  classifyWorkActionError,
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
    return { ok: true, page: await runtime.sdk.listWorks({ limit: 20 }) };
  } catch (error) {
    const known = classifyWorkActionError(error);
    if (known) return known;
    throw error;
  }
}
