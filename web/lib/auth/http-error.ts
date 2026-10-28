import { NextResponse } from "next/server";
import { RuntimeClientError } from "@/lib/runtime-client/errors";

export function authErrorResponse(error: unknown): NextResponse {
  return NextResponse.json(
    {
      error:
        error instanceof Error ? error.message : "Authentication unavailable.",
    },
    {
      status: error instanceof RuntimeClientError ? (error.status ?? 503) : 503,
    },
  );
}
