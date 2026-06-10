import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import {
  RuntimeClientError,
  requireRuntimeClient,
  runtimeErrorDetail,
} from "@/lib/runtime-client";

type RuntimeTodosResponse = {
  tasks?: Array<Record<string, unknown>>;
};

export async function GET(
  _request: Request,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

  const ownerUserId = auth.user.user_id;
  const { chatId } = await context.params;
  const chat = getChat(ownerUserId, chatId);
  if (!chat) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }

  const sessionId = chat.session?.backendSessionId ?? chatId;
  if (!sessionId) {
    return NextResponse.json({
      sessionId: null,
      tasks: [],
      events: [],
      generatedAt: new Date().toISOString(),
    });
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load web work surface",
    });
    const todos = await runtime.get<RuntimeTodosResponse>(
      `/sessions/${encodeURIComponent(sessionId)}/todos`,
      {
        auth: "required",
        operation: "load session todos for web work surface",
      },
    );
    const events = chat.activeRun?.runId
      ? await runtime.sdk.getRunEvents(chat.activeRun.runId, 0).catch(() => [])
      : [];

    return NextResponse.json({
      sessionId,
      tasks: Array.isArray(todos.tasks) ? todos.tasks : [],
      events,
      generatedAt: new Date().toISOString(),
    });
  } catch (error) {
    if (error instanceof RuntimeClientError) {
      return NextResponse.json(
        { error: runtimeErrorDetail(error) },
        { status: error.status || 502 },
      );
    }
    return NextResponse.json(
      {
        error:
          error instanceof Error
            ? error.message
            : "Failed to load work surface.",
      },
      { status: 502 },
    );
  }
}
