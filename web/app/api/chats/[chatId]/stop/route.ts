import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat, stopActiveRun, StaleRunBindingError } from "@/lib/api/web-store";
import type { StopRunRequest } from "@/lib/api/types";
import { RuntimeClientError } from "@/lib/runtime-client";

const STOP_CANCEL_ACK_TIMEOUT_MS = 1_500;

export async function POST(
  request: Request,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

  const body: unknown = await request.json().catch(() => null);
  const validId = (id: unknown): id is string => typeof id === "string" &&
    id.length > 0 && id.length <= 512 && id.trim() === id && !/[\x00-\x1f\x7f]/.test(id);
  if (!body || typeof body !== "object" || Array.isArray(body) ||
    !("sessionId" in body) || !validId(body.sessionId) ||
    !("runId" in body) || !validId(body.runId) ||
    !("assistantMessageId" in body) || body.assistantMessageId !== null && !validId(body.assistantMessageId)) {
    return NextResponse.json({ error: "a valid Stop run binding is required" }, { status: 400 });
  }
  const binding: StopRunRequest = {
    sessionId: body.sessionId, runId: body.runId, assistantMessageId: body.assistantMessageId,
  };

  const { chatId } = await context.params;
  const chat = getChat(auth.user.user_id, chatId);
  if (!chat) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  if (chat.chat.archivedAt) {
    return NextResponse.json(
      { error: "archived chat is read-only" },
      { status: 409 },
    );
  }
  if (!chat.activeRun?.runId) {
    return NextResponse.json(
      { error: "no active run is available to stop" },
      { status: 409 },
    );
  }

  try {
    const result = await stopActiveRun(auth.user.user_id, chatId, binding, {
      skipSync: true,
      cancelTimeoutMs: STOP_CANCEL_ACK_TIMEOUT_MS,
      client: auth.client,
    });
    if (!result) {
      return NextResponse.json({ error: "chat not found" }, { status: 404 });
    }
    return NextResponse.json(result);
  } catch (error) {
    if (error instanceof StaleRunBindingError) {
      return NextResponse.json({ error: error.message }, { status: 409 });
    }
    if (error instanceof RuntimeClientError && error.status) {
      return NextResponse.json(
        { error: error.detail },
        { status: error.status },
      );
    }
    const message =
      error instanceof Error ? error.message : "failed to stop active run";
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
