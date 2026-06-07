import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat, stopActiveRun } from "@/lib/api/web-store";
import { RuntimeClientError } from "@/lib/runtime-client";

export async function POST(
  _request: Request,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

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
    const result = await stopActiveRun(auth.user.user_id, chatId);
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
      error instanceof Error ? error.message : "failed to stop active run";
    return NextResponse.json({ error: message }, { status: 502 });
  }
}
