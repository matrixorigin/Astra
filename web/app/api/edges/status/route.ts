import { PATH_EDGES_STATUS } from "@astra/sdk";
import { NextResponse } from "next/server";
import type { EdgeStatusResponse } from "@/lib/api/types";
import { getRuntimeClient } from "@/lib/runtime-client";

export const dynamic = "force-dynamic";

const EMPTY_EDGE_STATUS: EdgeStatusResponse = { edges: [] };

export async function GET() {
  const runtime = await getRuntimeClient({
    auth: "optional",
    operation: "list edge executors",
  });
  if (!runtime) {
    return NextResponse.json(EMPTY_EDGE_STATUS);
  }

  try {
    return NextResponse.json(
      await runtime.get<EdgeStatusResponse>(PATH_EDGES_STATUS, {
        auth: "optional",
        operation: "list edge executors",
      }),
    );
  } catch {
    return NextResponse.json(EMPTY_EDGE_STATUS);
  }
}
