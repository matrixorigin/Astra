import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import { AstraApiError } from "@astra/sdk";
import {
  RuntimeClientError,
} from "@/lib/runtime-client";

type ApprovalBody = {
  requestId?: unknown;
  tool?: unknown;
  sessionId?: unknown;
  runId?: unknown;
  decision?: unknown;
  approvalKind?: unknown;
};

export async function POST(
  request: Request,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) return auth.response;

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

  let body: ApprovalBody;
  try {
    body = (await request.json()) as ApprovalBody;
  } catch {
    return NextResponse.json({ error: "invalid JSON body" }, { status: 400 });
  }
  const requestId = stringField(body.requestId);
  const tool = stringField(body.tool);
  const sessionId = stringField(body.sessionId);
  const runId = stringField(body.runId);
  const decision = body.decision === "allow" || body.decision === "deny"
    ? body.decision
    : null;
  const approvalKind = body.approvalKind === "protected_bash_write"
    ? "protected_bash_write"
    : body.approvalKind === "explicit"
    ? "explicit"
    : "standard";
  if (!requestId || !tool || !sessionId || !runId || !decision) {
    return NextResponse.json(
      { error: "requestId, tool, sessionId, runId, and decision are required" },
      { status: 400 },
    );
  }

  const activeRoot = chat.activeRun?.runId ?? null;
  if (chat.session?.backendSessionId !== sessionId) {
    return NextResponse.json(
      { error: "approval does not belong to this chat's active run" },
      { status: 409 },
    );
  }

  try {
    const runtime = auth.client;
    if (runId !== activeRoot) {
      // Only a durable descendant of this chat's active root may use this
      // route. If the root settled, the existing conversation selection owns
      // which root remains associated with this chat.
      const child = await runtime.sdk.getRunStatus(runId);
      const rootRunId = activeRoot ??
        (await runtime.sdk.getSessionConversation(sessionId, 1)).selected_root?.run_id;
      const current = getChat(auth.user.user_id, chatId);
      if (
        !rootRunId || child.runId !== runId || child.sessionId !== sessionId ||
        !child.parentRunId || child.rootRunId !== rootRunId ||
        current?.chat.archivedAt || current?.session?.backendSessionId !== sessionId ||
        (current?.activeRun?.runId ?? null) !== activeRoot
      ) {
        return NextResponse.json(
          { error: "approval does not belong to this chat's active run" },
          { status: 409 },
        );
      }
    }
    await runtime.post(
      "/approval/respond",
      {
        request_id: requestId,
        decision,
        reason: decision === "deny" ? "Declined in the web review surface" : null,
        session_id: sessionId,
        run_id: runId,
        tool_name: tool,
        approval_kind: approvalKind,
      },
      { auth: "required", operation: "respond to run approval" },
    );
    return NextResponse.json({ status: decision === "allow" ? "approved" : "denied" });
  } catch (error) {
    if (error instanceof AstraApiError) {
      return NextResponse.json(
        { error: error.status === 404 ? "The approval run is no longer available." : "The approval run could not be checked. Retry." },
        { status: error.status === 404 ? 409 : error.status },
      );
    }
    if (error instanceof RuntimeClientError && error.status) {
      return NextResponse.json({ error: error.detail }, { status: error.status });
    }
    return NextResponse.json(
      { error: error instanceof Error ? error.message : "approval response failed" },
      { status: 502 },
    );
  }
}

function stringField(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value.trim() : null;
}
