import { NextResponse } from "next/server";
import { runtimeModelCatalog } from "@/lib/api/model-catalog";
import { requireRuntimeClient } from "@/lib/runtime-client";

export const dynamic = "force-dynamic";

export async function GET() {
  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "list runtime models",
    });
    return NextResponse.json(await runtimeModelCatalog(runtime));
  } catch (error) {
    return NextResponse.json(
      {
        error: "model_access_unavailable",
        detail:
          error instanceof Error
            ? error.message
            : "Failed to load Model Access.",
        action: "sign_in_or_retry",
      },
      { status: 503 },
    );
  }
}
