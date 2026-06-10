import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import {
  RuntimeClientError,
  requireRuntimeClient,
  runtimeErrorDetail,
} from "@/lib/runtime-client";

type RuntimeRunProjectionResponse = {
  run_id?: string;
  session_id?: string;
  status?: string;
  recent_events?: Array<Record<string, unknown>>;
};

const AGENT_RUN_RECENT_EVENT_LIMIT = 120;

export async function GET(
  _request: Request,
  context: { params: Promise<{ chatId: string; runId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }

  const ownerUserId = auth.user.user_id;
  const { chatId, runId } = await context.params;
  const chat = getChat(ownerUserId, chatId);
  if (!chat) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load agent run projection",
    });
    const projection = await runtime.get<RuntimeRunProjectionResponse>(
      `/chat/runs/${encodeURIComponent(
        runId,
      )}/projection?recent_limit=${AGENT_RUN_RECENT_EVENT_LIMIT}`,
      {
        auth: "required",
        operation: "load agent run projection",
      },
    );

    return NextResponse.json({
      runId: projection.run_id ?? runId,
      sessionId: projection.session_id ?? null,
      status: projection.status ?? null,
      events: projection.recent_events ?? [],
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
            : "Failed to load agent run projection.",
      },
      { status: 502 },
    );
  }
}
