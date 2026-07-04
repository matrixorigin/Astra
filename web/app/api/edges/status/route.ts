import { PATH_EDGES_STATUS } from "@astra/sdk";
import { NextResponse } from "next/server";
import type { EdgeStatusResponse } from "@/lib/api/types";
import { requireRuntimeClient } from "@/lib/runtime-client";

export const dynamic = "force-dynamic";

const EMPTY_EDGE_STATUS: EdgeStatusResponse = { edges: [] };

function runtimeErrorStatus(error: unknown): number | null {
  if (
    typeof error === "object" &&
    error !== null &&
    "status" in error &&
    typeof error.status === "number"
  ) {
    return error.status;
  }
  return null;
}

function runtimeErrorMessage(error: unknown): string | null {
  if (
    typeof error === "object" &&
    error !== null &&
    "detail" in error &&
    typeof error.detail === "string"
  ) {
    return error.detail;
  }
  if (error instanceof Error && error.message) {
    return error.message;
  }
  return null;
}

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "list edge executors",
    });
    return NextResponse.json(
      await runtime.get<EdgeStatusResponse>(PATH_EDGES_STATUS, {
        auth: "required",
        operation: "list edge executors",
      }),
    );
  } catch (error) {
    const status = runtimeErrorStatus(error);
    if (status === 401 || status === 403) {
      return NextResponse.json(
        { error: runtimeErrorMessage(error) ?? "Authentication required." },
        { status },
      );
    }
    return NextResponse.json(EMPTY_EDGE_STATUS);
  }
}
