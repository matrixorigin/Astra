import { NextRequest, NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat, queueDeferredRunInput } from "@/lib/api/web-store";
import type { SendMessageRequest } from "@/lib/api/types";
import { RuntimeClientError } from "@/lib/runtime-client";

const MAX_DEFERRED_INPUT_CHARS = 20_000;

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

  const { chatId } = await context.params;
  const body = (await request.json()) as SendMessageRequest;
  if (!body.content?.trim()) {
    return NextResponse.json({ error: "content is required" }, { status: 400 });
  }
  if ([...body.content].length > MAX_DEFERRED_INPUT_CHARS) {
    return NextResponse.json(
      { error: "deferred input is too large" },
      { status: 413 },
    );
  }

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
      { error: "no active run is available for deferred input" },
      { status: 409 },
    );
  }

  try {
    const result = await queueDeferredRunInput(auth.user.user_id, chatId, body);
    if (!result) {
      return NextResponse.json({ error: "chat not found" }, { status: 404 });
    }
    return NextResponse.json(result);
  } catch (error) {
    if (error instanceof RuntimeClientError && error.status) {
      return NextResponse.json(
        { error: error.detail },
        { status: error.status },
      );
    }
    const message =
      error instanceof Error
        ? error.message
        : "failed to submit deferred run input";
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
