import { NextRequest, NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage, claimChatStreamProjection, ChatStreamProjectionError,
  ensureChatBackendSession, getChat, normalizedActiveSkills, normalizedActiveTools,
  releaseChatStreamProjection, requireSelectedOfferingId, resolveModelOfferingSelection,
  setChatActiveRun, withChatStreamProjection,
} from "@/lib/api/web-store";
import type { SendMessageRequest } from "@/lib/api/types";
import { applyStreamEvent } from "@/lib/api/stream-event-handler";
import { streamObservationInterrupted } from "@/lib/api/errors";
import { RuntimeClientError, readRuntimeErrorDetail, runtimeErrorDetail } from "@/lib/runtime-client/errors";
import { chatRunStreamPath, parseSseDataEvents } from "@astra/sdk";

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
  const ownerUserId = auth.user.user_id;
  const model = requireSelectedOfferingId(body.options?.model ?? chat.chat.model);
  let started;
  try {
    started = beginStreamingMessage(ownerUserId, chatId, body);
  } catch (error) {
    if (!(error instanceof ChatStreamProjectionError)) throw error;
    return NextResponse.json({ error: error.message, code: error.code }, { status: 409 });
  }
  if (!started) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const controller = new AbortController();
  // Transport deadlines must still let the owning writer settle its message.
  const deadline = new AbortController();
  const signal = AbortSignal.any([controller.signal, deadline.signal]);
  const lease = claimChatStreamProjection(ownerUserId, chatId, started.assistantMessage.id,
    controller, { sessionId: chat.session?.backendSessionId ?? "" });
  const state = lease.projection.state;
  const ctx = { ownerUserId, chatId, assistantMessageId: started.assistantMessage.id,
    getSessionId: () => lease.projection.sessionId };
  const assertCurrent = () => {
    if (!withChatStreamProjection(lease, () => {})) throw streamObservationInterrupted();
    signal.throwIfAborted();
  };
  const result = () => NextResponse.json({
    userMessage: started.userMessage, assistantMessage: started.assistantMessage,
  });
  let timeout = setTimeout(() => deadline.abort(new Error("Run admission timed out.")), 30_000);
  try {
    const runtime = auth.client;
    assertCurrent();
    const sessionId = await ensureChatBackendSession(ownerUserId, chatId, { model, runtime });
    assertCurrent();
    lease.projection.sessionId = sessionId;
    const modelSelection = await resolveModelOfferingSelection(runtime, model);
    assertCurrent();
    const activeSkills = normalizedActiveSkills(body.options?.activeSkills);
    const activeTools = normalizedActiveTools(body.options?.activeTools, body.options?.webSearch);
    const run = await runtime.sdk.createRun({
      message: body.content, sessionId, modelSelection,
      allowSkills: activeSkills.length ? activeSkills : undefined, enabledTools: activeTools,
      context: { source: "web_v1", edge_profile: activeSkills.length || activeTools.length
        ? { ...(activeSkills.length ? { active_skills: activeSkills } : {}),
          ...(activeTools.length ? { active_tools: activeTools } : {}) } : undefined },
    }, { signal });
    // Retain an acknowledged identity even if the transport deadline just fired.
    // Replayed events, rather than the admission ACK, own terminal state.
    if (!withChatStreamProjection(lease, () => {
      if (!run.runId || run.sessionId !== sessionId) {
        throw new Error("Run admission returned a conflicting Session or missing Run identity.");
      }
      state.runId = run.runId;
      setChatActiveRun(ownerUserId, chatId, { runId: run.runId, status: run.status,
        waitingFor: run.waitingFor, assistantMessageId: started.assistantMessage.id, nextEventIndex: state.nextEventIndex ?? 0 });
    })) throw streamObservationInterrupted();
    assertCurrent();
    clearTimeout(timeout);
    timeout = setTimeout(() => deadline.abort(new Error("Run observation timed out.")), 180_000);
    const response = await runtime.fetchResponse(
      `${chatRunStreamPath(run.runId)}?last_index=${state.nextEventIndex ?? 0}`,
      { auth: "required", operation: `stream run ${run.runId}`, signal },
    );
    assertCurrent();
    if (!response.ok) throw new Error(await readRuntimeErrorDetail(response));
    const text = await response.text();
    assertCurrent();
    for (const event of parseSseDataEvents(text)) {
      if (!withChatStreamProjection(lease, () => applyStreamEvent(event, ctx, state))) {
        throw streamObservationInterrupted();
      }
    }
    if (state.protocolError || state.lastStatus === "streaming" && state.runLifecycle === "running") {
      throw streamObservationInterrupted();
    }
    return result();
  } catch (error) {
    // An admitted Run survives observation loss; callers can resume its cursor.
    if (state.runId || !withChatStreamProjection(lease, () => {})) {
      const interrupted = streamObservationInterrupted();
      return NextResponse.json({ error: interrupted.detail, code: interrupted.code }, { status: interrupted.status });
    }
    const prefix = error instanceof RuntimeClientError
      ? "Astra runtime rejected the request" : "I could not reach the Astra runtime from the web UI";
    withChatStreamProjection(lease, () => applyStreamEvent({ type: "error",
      message: `${prefix}. (${runtimeErrorDetail(error, "unknown error")})` }, ctx, state));
    return result();
  } finally {
    clearTimeout(timeout);
    releaseChatStreamProjection(lease);
  }
}
