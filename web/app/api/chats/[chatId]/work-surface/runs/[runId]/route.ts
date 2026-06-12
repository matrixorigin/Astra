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
  workspace?: Record<string, unknown> | null;
  executor?: Record<string, unknown> | null;
  transport?: string | null;
  fallback_policy?: string | null;
  recent_events?: Array<Record<string, unknown>>;
};

const AGENT_RUN_RECENT_EVENT_LIMIT = 120;

function projectionBindingSeedEvent(
  projection: RuntimeRunProjectionResponse,
) {
  if (!projection.workspace && !projection.executor) {
    return null;
  }
  return {
    type: "run_started",
    run_id: projection.run_id,
    session_id: projection.session_id,
    status: projection.status,
    workspace: projection.workspace ?? undefined,
    executor: projection.executor ?? undefined,
    transport: projection.transport ?? undefined,
    fallback_policy: projection.fallback_policy ?? undefined,
  };
}

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

    const bindingSeed = projectionBindingSeedEvent(projection);
    return NextResponse.json({
      runId: projection.run_id ?? runId,
      sessionId: projection.session_id ?? null,
      status: projection.status ?? null,
      workspace: projection.workspace ?? null,
      executor: projection.executor ?? null,
      transport: projection.transport ?? null,
      fallbackPolicy: projection.fallback_policy ?? null,
      events: [
        ...(bindingSeed ? [bindingSeed] : []),
        ...(projection.recent_events ?? []),
      ],
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
