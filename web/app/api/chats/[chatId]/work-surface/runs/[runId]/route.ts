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
    let transcript: Awaited<
      ReturnType<typeof runtime.sdk.getSessionTranscript>
    > | null = null;
    let transcriptWarning: string | null = null;
    if (projection.session_id) {
      try {
        transcript = await runtime.sdk.getSessionTranscript(
          projection.session_id,
          { limit: 500 },
        );
      } catch (error) {
        transcriptWarning = `Canonical transcript unavailable: ${runtimeErrorDetail(error)}`;
      }
    } else {
      transcriptWarning =
        "Canonical transcript unavailable because the child run did not report a session identity.";
    }

    const bindingSeed = projectionBindingSeedEvent(projection);
    const runTranscript =
      transcript?.items.filter((item) => item.run_id === (projection.run_id ?? runId)) ??
      [];
    if (!transcriptWarning && transcript?.has_more) {
      transcriptWarning =
        "Showing the latest transcript window; older session items were not loaded.";
    }
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
      transcript: runTranscript,
      transcriptComplete: Boolean(transcript && !transcript.has_more),
      transcriptWarning,
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
