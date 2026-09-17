import {
  ASTRA_WORK_API_MAJOR,
  ASTRA_WORK_API_MAJOR_HEADER,
  workBranchTurnsPath,
} from "@astra/sdk";
import { NextRequest, NextResponse } from "next/server";
import {
  RuntimeClientError,
  requireRuntimeClient,
} from "@/lib/runtime-client";

const MAX_TURN_BODY_BYTES = 263 * 1024;
const MAX_MESSAGE_BYTES = 256 * 1024;
const MAX_REQUEST_ID_BYTES = 256;
const encoder = new TextEncoder();

type WorkTurnBody = { request_id: string; attachment_id: string; message: string };

function invalidRequest(detail: string, status = 400) {
  return NextResponse.json(
    {
      code: "invalid_work_turn_request",
      category: "invalid_request",
      retryable: false,
      action_hints: [],
      detail,
    },
    { status },
  );
}

function decodeTurnBody(raw: string): WorkTurnBody | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const object = value as Record<string, unknown>;
  const fields = Object.keys(object).sort();
  if (
    fields.length !== 3 ||
    fields[0] !== "attachment_id" ||
    fields[1] !== "message" ||
    fields[2] !== "request_id" ||
    typeof object.request_id !== "string" ||
    typeof object.attachment_id !== "string" ||
    typeof object.message !== "string"
  ) {
    return null;
  }
  const requestIdBytes = encoder.encode(object.request_id).length;
  const attachmentIdBytes = encoder.encode(object.attachment_id).length;
  const messageBytes = encoder.encode(object.message).length;
  if (
    requestIdBytes < 1 ||
    requestIdBytes > MAX_REQUEST_ID_BYTES ||
    /\p{Cc}/u.test(object.request_id) ||
    attachmentIdBytes < 1 ||
    attachmentIdBytes > 128 ||
    /\p{Cc}/u.test(object.attachment_id) ||
    object.message.trim().length === 0 ||
    messageBytes > MAX_MESSAGE_BYTES
  ) {
    return null;
  }
  return {
    request_id: object.request_id,
    attachment_id: object.attachment_id,
    message: object.message,
  };
}

async function readBoundedBody(
  request: NextRequest,
  maxBytes: number,
): Promise<string | null> {
  const declared = request.headers.get("content-length");
  if (declared) {
    const size = Number(declared);
    if (Number.isFinite(size) && size > maxBytes) return null;
  }
  if (!request.body) return "";

  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maxBytes) {
        await reader.cancel();
        return null;
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const body = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(body);
}

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ workId: string; branchId: string }> },
) {
  let raw: string | null;
  try {
    raw = await readBoundedBody(request, MAX_TURN_BODY_BYTES);
  } catch {
    return invalidRequest("Work turn request must be valid UTF-8.");
  }
  if (raw === null) {
    return invalidRequest("Work turn request is too large.", 413);
  }
  const input = decodeTurnBody(raw);
  if (!input) {
    return invalidRequest(
      "Work turn request must contain one bounded request_id, attachment_id, and message.",
    );
  }

  const { workId, branchId } = await context.params;
  let path: string;
  try {
    path = workBranchTurnsPath(workId, branchId);
  } catch (error) {
    if (error instanceof TypeError) {
      return invalidRequest("Work or branch identity is invalid.");
    }
    throw error;
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "continue Work",
    });
    const backend = await runtime.fetchResponse(path, {
      method: "POST",
      auth: "required",
      body: JSON.stringify(input),
      signal: request.signal,
      headers: {
        Accept: "text/event-stream",
        "Content-Type": "application/json",
        [ASTRA_WORK_API_MAJOR_HEADER]: ASTRA_WORK_API_MAJOR,
      },
    });

    if (!backend.ok) {
      return new Response(await backend.arrayBuffer(), {
        status: backend.status,
        headers: {
          "Content-Type": backend.headers.get("content-type") ?? "application/json",
          "Cache-Control": "no-store",
        },
      });
    }
    if (!backend.body) {
      return NextResponse.json(
        {
          code: "work_turn_unavailable",
          category: "availability",
          retryable: true,
          action_hints: ["retry_write"],
          detail: "Work turn stream was unavailable.",
        },
        { status: 502 },
      );
    }

    return new Response(backend.body, {
      status: 200,
      headers: {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache, no-transform",
        Connection: "keep-alive",
        "X-Accel-Buffering": "no",
      },
    });
  } catch (error) {
    if (error instanceof RuntimeClientError) {
      return NextResponse.json(
        {
          code: error.code ?? "work_turn_unavailable",
          category: error.status === 401 ? "authentication" : "availability",
          retryable: (error.status ?? 500) >= 500,
          action_hints: [],
          detail:
            error.status === 401
              ? "Runtime authentication is required."
              : "Work turn is temporarily unavailable.",
        },
        { status: error.status ?? 500 },
      );
    }
    if (error instanceof TypeError) {
      return NextResponse.json(
        {
          code: "work_turn_unavailable",
          category: "availability",
          retryable: true,
          action_hints: ["retry_write"],
          detail: "Work turn transport is temporarily unavailable.",
        },
        { status: 503 },
      );
    }
    throw error;
  }
}
