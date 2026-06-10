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

type RuntimeRunProjectionResponse = {
  session_id?: string;
  recent_events?: Array<Record<string, unknown>>;
};

const WORK_SURFACE_RECENT_EVENT_LIMIT = 400;

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

  const sessionId = chat.session?.backendSessionId ?? null;
  const runId = chat.activeRun?.runId ?? null;
  if (!sessionId && !runId) {
    return NextResponse.json({
      sessionId: null,
      runId,
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
    const warnings: string[] = [];
    const projection = runId
      ? await runtime
          .get<RuntimeRunProjectionResponse>(
            `/chat/runs/${encodeURIComponent(
              runId,
            )}/projection?recent_limit=${WORK_SURFACE_RECENT_EVENT_LIMIT}`,
            {
              auth: "required",
              operation: "load active run projection for web work surface",
            },
          )
          .catch((error: unknown) => {
            warnings.push(
              `Run activity is temporarily unavailable: ${runtimeErrorDetail(
                error,
              )}`,
            );
            return null;
          })
      : null;
    const resolvedSessionId = sessionId ?? projection?.session_id ?? null;
    const todos = resolvedSessionId
      ? await runtime
          .get<RuntimeTodosResponse>(
            `/sessions/${encodeURIComponent(resolvedSessionId)}/todos`,
            {
              auth: "required",
              operation: "load session todos for web work surface",
            },
          )
          .catch((error: unknown) => {
            warnings.push(
              `Tasks are temporarily unavailable: ${runtimeErrorDetail(error)}`,
            );
            return { tasks: [] };
          })
      : { tasks: [] };
    const events = projection?.recent_events ?? [];

    return NextResponse.json({
      sessionId: resolvedSessionId,
      runId,
      tasks: Array.isArray(todos.tasks) ? todos.tasks : [],
      events,
      warnings,
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
