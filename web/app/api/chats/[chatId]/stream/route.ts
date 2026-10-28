import { parseChatSseFrame, isReplayConnectionError } from "@/lib/api/chat-sse-frame";
import { NextRequest, NextResponse } from "next/server";
import {
  ASTRA_AGENT_INTERACTION_API_MAJOR,
  ASTRA_AGENT_INTERACTION_API_MAJOR_HEADER,
  PATH_CHAT_STREAM,
  buildQueryString,
  chatRunStreamPath,
} from "@astra/sdk";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { WebApiError, streamObservationInterrupted } from "@/lib/api/errors";
import {
  beginStreamingMessage,
  claimChatStreamProjection,
  normalizedActiveSkills,
  normalizedActiveTools,
  withChatStreamProjection,
  releaseChatStreamProjection,
  ChatStreamProjectionError,
  type ChatStreamProjectionLease,
  ensureChatBackendSession,
  getChat,
  resolveModelOfferingSelection,
  requireSelectedOfferingId,
  setChatActiveRun as setStoredActiveRun,
  updateChatWorkspaceSelection,
  updateStreamingAssistantMessage as updateStoredAssistant,
} from "@/lib/api/web-store";
import { fetchSessionArtifacts } from "@/lib/api/stream-artifacts";
import {
  applyStreamEvent,
  applyExplainAnalyzeObservation,
  type StreamEventContext,
  type StreamEventState,
} from "@/lib/api/stream-event-handler";
import {
  RuntimeClientError,
  WebRuntimeClient,
  readRuntimeErrorDetail,
} from "@/lib/runtime-client";
import type {
  SendMessageRequest,
} from "@/lib/api/types";
import {
  normalizeWorkspaceSelection,
  resolveWorkspaceBindings,
  sameWorkspaceSelection,
  validateWorkspaceAuthority,
} from "@/lib/workspace-authority";
import { verifyLiveWorkspaceSelection } from "@/lib/workspace-selection-server";

const encoder = new TextEncoder();

function sseFrame(event: unknown) {
  return encoder.encode(`data: ${JSON.stringify(event)}\n\n`);
}

function eventFromSseFrame(frame: string, strict = false): import("@astra/sdk").StreamEvent | null {
  return parseChatSseFrame(frame, strict) as import("@astra/sdk").StreamEvent | null;
}

async function readSendMessageRequest(
  request: NextRequest,
  chat: ReturnType<typeof getChat>,
) {
  const rawBody = await request.text();
  if (rawBody.trim()) {
    try {
      return JSON.parse(rawBody) as SendMessageRequest;
    } catch {
      return null;
    }
  }
  if (!chat?.pendingTurn) {
    return null;
  }
  return {
    content: chat.pendingTurn.content,
    options: chat.pendingTurn.options,
    pendingMessageId: chat.pendingTurn.messageId,
    ...(chat.workspaceSelection ? { workspace: chat.workspaceSelection } : {}),
  } satisfies SendMessageRequest;
}

async function readErrorDetail(response: Response) {
  return readRuntimeErrorDetail(response);
}

function hasMessagesBeforePendingTurn(
  chat: NonNullable<ReturnType<typeof getChat>>,
) {
  const pendingMessageId = chat.pendingTurn?.messageId;
  return chat.messages.some((message) => message.id !== pendingMessageId);
}

function lastAssistantMessageId(messages: Array<{ id: string; role: string }>) {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index]?.role === "assistant") {
      return messages[index]?.id ?? null;
    }
  }
  return null;
}

function assistantMessageBelongsToChat(
  messages: Array<{ id: string; role: string }>,
  messageId: string | null | undefined,
) {
  if (!messageId) {
    return null;
  }
  return messages.some(
    (message) => message.id === messageId && message.role === "assistant",
  )
    ? messageId
    : null;
}

function normalizedLastIndex(value: string | null) {
  if (!value) {
    return null;
  }
  const parsed = Number(value);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return null;
  }
  return Math.trunc(parsed);
}

function isMalformedStreamEventIndexError(error: unknown) {
  return (
    error instanceof Error && error.message === "Malformed stream event index."
  );
}

function applyBackendStreamEvent(
  event: import("@astra/sdk").StreamEvent,
  ctx: StreamEventContext,
  state: StreamEventState,
) {
  try {
    applyStreamEvent(event, ctx, state);
  } catch (error) {
    if (isMalformedStreamEventIndexError(error)) {
      return;
    }
    throw error;
  }
}

function proxyRunStream(params: {
  backendResponse:
    | Response
    | ((emit: (event: unknown) => void) => Promise<Response>);
  backendAbortController: AbortController;
  ownerUserId: string;
  chatId: string;
  sessionId: string | (() => string);
  runtime: WebRuntimeClient | (() => Promise<WebRuntimeClient>);
  assistantMessageId: string;
  projectionLease?: ChatStreamProjectionLease;
  knownArtifactIds: Set<string>;
  replayOnly?: boolean;
  replayFromOrigin?: boolean;
  localMessages?: {
    userMessage: unknown;
    assistantMessage: unknown;
  };
}) {
  const interactionProtocolHeader = ASTRA_AGENT_INTERACTION_API_MAJOR_HEADER;
  const expectedInteractionProtocol = ASTRA_AGENT_INTERACTION_API_MAJOR;
  const {
    backendResponse,
    backendAbortController,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId,
    projectionLease,
    knownArtifactIds,
    replayOnly = false,
    replayFromOrigin = false,
    localMessages,
  } = params;
  const currentSessionId =
    typeof sessionId === "function" ? sessionId : () => sessionId;
  const currentRuntime =
    typeof runtime === "function" ? runtime : () => Promise.resolve(runtime);

  let lease = projectionLease;
  if (!replayOnly && !lease) {
    try {
      lease = claimChatStreamProjection(ownerUserId, chatId, assistantMessageId,
        backendAbortController, { sessionId: "" });
    } catch (error) {
      if (!(error instanceof ChatStreamProjectionError)) throw error;
      return NextResponse.json({ error: error.message, code: error.code }, { status: 409 });
    }
  }
  const mutate = (apply: () => void) => lease ? withChatStreamProjection(lease, apply) : (apply(), true);
  const updateStreamingAssistantMessage: typeof updateStoredAssistant = (...args) => {
    let result: ReturnType<typeof updateStoredAssistant> = null;
    mutate(() => { result = updateStoredAssistant(...args); });
    return result;
  };
  const setChatActiveRun: typeof setStoredActiveRun = (...args) => {
    let result: ReturnType<typeof setStoredActiveRun>;
    mutate(() => { result = setStoredActiveRun(...args); });
    return result;
  };

  let backendReader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  let clientCancelled = false;

  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const enqueueFrame = (event: unknown) => {
        if (clientCancelled || !mutate(() => {})) return false;
        try {
          controller.enqueue(sseFrame(event));
          return true;
        } catch {
          clientCancelled = true;
          backendAbortController.abort();
          return false;
        }
      };
      const interruptObservation = () => {
        const error = streamObservationInterrupted();
        enqueueFrame({ type: "error", message: error.detail, status: error.status, code: error.code });
      };
      const closeController = () => {
        if (clientCancelled) {
          return;
        }
        try {
          controller.close();
        } catch {
          clientCancelled = true;
        }
      };

      if (localMessages) {
        enqueueFrame({
          type: "local_messages",
          user_message: localMessages.userMessage,
          assistant_message: localMessages.assistantMessage,
        });
      }

      const runProxy = async () => {
        const state: StreamEventState = lease?.projection.state ?? {
          assistantText: "", assistantRawText: "", reasoningText: "",
          lastStatus: "streaming", protocolError: false, runLifecycle: "running",
        };
        if (projectionLease) {
          const { assistantRawText, assistantText, reasoningText, reasoningFromThinkingTags,
            runId, nextEventIndex, lastStatus, runLifecycle, lastFailureMessage, errorCode, terminalStatus } = state;
          if (!enqueueFrame({ type: "chat_stream_init", assistant_message_id: assistantMessageId,
            session_id: currentSessionId(), state: { assistantRawText, assistantText, reasoningText,
              reasoningFromThinkingTags, runId, nextEventIndex, lastStatus, runLifecycle,
              lastFailureMessage, errorCode, terminalStatus } })) return;
        }
        let resolvedBackendResponse: Response;
        try {
          resolvedBackendResponse =
            typeof backendResponse === "function"
              ? await backendResponse((event) => {
                  enqueueFrame(event);
                })
              : backendResponse;
          mutate(() => {
            if (!lease) return;
            const sessionId = currentSessionId();
            if (lease.projection.sessionId && lease.projection.sessionId !== sessionId) {
              throw new Error("The stream session binding changed.");
            }
            lease.projection.sessionId = sessionId;
          });
        } catch (error) {
          if (clientCancelled) {
            return;
          }
          const message =
            error instanceof Error ? error.message : "Astra stream failed.";
          const status =
            error instanceof RuntimeClientError ? error.status : undefined;
          const code =
            error instanceof RuntimeClientError ? error.code : undefined;
          if (!replayOnly && projectionLease) {
            if (state.lastStatus === "streaming" && state.runLifecycle === "running") {
              interruptObservation();
            }
            closeController();
            return;
          }
          if (!replayOnly) {
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: message,
                status: "failed",
              },
            );
            setChatActiveRun(ownerUserId, chatId, undefined);
          }
          enqueueFrame({ type: "error", message, status, code });
          closeController();
          return;
        }
        if (clientCancelled || !mutate(() => {})) { closeController(); return; }
        if (!resolvedBackendResponse.ok || !resolvedBackendResponse.body) {
          if (!replayOnly && projectionLease) {
            if (state.lastStatus === "streaming" && state.runLifecycle === "running") {
              interruptObservation();
            }
            backendAbortController.abort();
            closeController();
            return;
          }
          const detail = await readErrorDetail(resolvedBackendResponse);
          if (clientCancelled) {
            return;
          }
          if (!replayOnly) {
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: detail,
                status: "failed",
              },
            );
            setChatActiveRun(ownerUserId, chatId, undefined);
          }
          enqueueFrame({ type: "error", message: detail });
          closeController();
          return;
        }
        const actualInteractionProtocol = resolvedBackendResponse.headers.get(
          interactionProtocolHeader,
        );
        if (actualInteractionProtocol !== expectedInteractionProtocol) {
          const message = actualInteractionProtocol
            ? `Astra Server interaction protocol ${actualInteractionProtocol} is incompatible with Web ${expectedInteractionProtocol}. Restart or upgrade the Server.`
            : "Astra Server is missing the interaction protocol contract. Restart or upgrade the Server before starting another turn.";
          if (!replayOnly) {
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              { content: message, status: "failed" },
            );
            setChatActiveRun(ownerUserId, chatId, undefined);
          }
          mutate(() => { if (lease) lease.projection.state.protocolError = true; });
          enqueueFrame({
            type: "error",
            code: "RUNTIME_PROTOCOL_MISMATCH",
            message,
          });
          backendAbortController.abort();
          closeController();
          return;
        }

        const reader = resolvedBackendResponse.body.getReader();
        if (!reader) {
          enqueueFrame({
            type: "error",
            message: "Astra stream body is unavailable.",
          });
          closeController();
          return;
        }
        backendReader = reader;

        const decoder = new TextDecoder();
        let buffer = "";

        const ctx: StreamEventContext = {
          ownerUserId,
          chatId,
          assistantMessageId,
          getSessionId: currentSessionId,
        };

        const repairToken = replayOnly && replayFromOrigin ? crypto.randomUUID() : null;
        let sawExplainFact = false;
        let replayFailed = false;
        if (repairToken) updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId,
          { explainAnalyzeRepair: { token: repairToken, complete: false } });
        const observeReplay = (event: import("@astra/sdk").StreamEvent) => {
          sawExplainFact ||= event.type === "explain_analyze";
          replayFailed ||= isReplayConnectionError(event);
          applyExplainAnalyzeObservation(event, ctx, true);
        };

        const forwardFrame = (frame: string) => {
          const event = eventFromSseFrame(frame, replayOnly);
          if ((event as { type?: string } | null)?.type === "chat_stream_init") {
            mutate(() => { throw new Error("Runtime supplied a reserved Web initialization frame."); });
          }
          const duplicate = !replayOnly && event && typeof event.index === "number" &&
            Number.isSafeInteger(event.index) && event.index >= 0 &&
            event.index < (state.nextEventIndex ?? 0) && event.type !== "session_info";
          if (duplicate) return true;
          if (!mutate(() => {
            if (!event) return;
            if (replayOnly) { observeReplay(event); return; }
            applyBackendStreamEvent(event, ctx, state);
          })) return false;
          if (clientCancelled) return false;
          controller.enqueue(encoder.encode(`${frame}\n\n`));
          return true;
        };

        let upstreamDone = false;
        try {
          for (;;) {
            let chunk: ReadableStreamReadResult<Uint8Array>;
            try {
              chunk = await reader.read();
            } catch (error) {
              if (clientCancelled) return;
              // Transport loss cannot override an observed failure or resumable/terminal state.
              if (!replayOnly && (state.lastStatus === "failed" || state.runLifecycle !== "running")) {
                break;
              }
              if (replayOnly) throw error;
              interruptObservation();
              return;
            }
            const { value, done } = chunk;
            if (clientCancelled) {
              return;
            }
            if (done) {
              upstreamDone = true;
              break;
            }
            buffer += decoder.decode(value, { stream: true });

            const frames = buffer.split(/\r?\n\r?\n/);
            buffer = frames.pop() ?? "";
            for (const frame of frames) {
              if (!forwardFrame(frame)) return;
            }
          }

          {
            const tail = decoder.decode();
            if (tail) {
              buffer += tail;
            }
            if (buffer.trim() && !forwardFrame(buffer)) return;
          }

          if (clientCancelled) {
            return;
          }

          if (repairToken && sawExplainFact && !replayFailed) {
            updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId,
              { explainAnalyzeRepair: { token: repairToken, complete: true } });
          }

          if (!replayOnly && state.lastStatus === "streaming" && state.runLifecycle === "running") {
            interruptObservation();
            return;
          }

          if (!replayOnly && state.lastStatus === "complete") {
            const runtimeClient = await currentRuntime();
            const artifacts = (
              await fetchSessionArtifacts(runtimeClient, currentSessionId())
            ).filter((artifact) => !knownArtifactIds.has(artifact.id));
            if (clientCancelled) {
              return;
            }
            if (artifacts.length > 0) {
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                { artifacts },
              );
              enqueueFrame({ type: "artifacts", artifacts });
            }
          }

        } catch (error) {
          if (clientCancelled) {
            return;
          }
          const message =
            error instanceof Error ? error.message : "Astra stream failed.";
          if (error instanceof WebApiError && error.code === "contract_violation") {
            mutate(() => { state.protocolError = true; });
          }
          if (!replayOnly) {
            setChatActiveRun(ownerUserId, chatId, undefined);
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: state.assistantText || message,
                status: "failed",
              },
            );
          }
          if (replayOnly) updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId,
            { explainAnalyzeDegraded: true });
          enqueueFrame({ type: "error", message,
            ...(error instanceof WebApiError ? { status: error.status, code: error.code } : {}) });
        } finally {
          if (!upstreamDone) {
            backendAbortController.abort();
            await Promise.resolve(reader.cancel()).catch(() => undefined);
          }
          if (lease) releaseChatStreamProjection(lease);
          backendReader = null;
          reader.releaseLock();
          if (!clientCancelled) {
            closeController();
          }
        }
      };
      void runProxy().finally(() => { if (lease) releaseChatStreamProjection(lease); });
    },
    async cancel() {
      clientCancelled = true;
      backendAbortController.abort();
      await backendReader?.cancel();
    },
  });

  return new Response(stream, {
    headers: {
      "Content-Type": "text/event-stream; charset=utf-8",
      "Cache-Control": "no-store, no-transform",
      Connection: "keep-alive",
    },
  });
}

export async function POST(
  request: NextRequest,
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
  if (chat.chat.archivedAt) {
    return NextResponse.json(
      { error: "archived chat is read-only" },
      { status: 409 },
    );
  }

  const body = await readSendMessageRequest(request, chat);
  if (!body) {
    return NextResponse.json(
      { error: "invalid request body" },
      { status: 400 },
    );
  }
  if (!body.content?.trim()) {
    return NextResponse.json({ error: "content is required" }, { status: 400 });
  }

  const activeSkills = normalizedActiveSkills(body.options?.activeSkills);
  const activeTools = normalizedActiveTools(
    body.options?.activeTools,
    body.options?.webSearch,
  );
  const getStreamRuntime = async () => auth.client;
  const hasRequestedWorkspace = Object.prototype.hasOwnProperty.call(
    body,
    "workspace",
  );
  const storedWorkspaceSelection = normalizeWorkspaceSelection(
    chat.workspaceSelection,
  );
  const requestedWorkspaceSelection = normalizeWorkspaceSelection(
    body.workspace,
  );
  if (hasRequestedWorkspace && !requestedWorkspaceSelection) {
    return NextResponse.json(
      {
        error: "workspace must be a valid file environment selection",
        code: "invalid_workspace_selection",
      },
      { status: 400 },
    );
  }
  const workspaceSelection =
    requestedWorkspaceSelection ?? storedWorkspaceSelection;
  const workspaceAuthorityError = validateWorkspaceAuthority(
    body.content,
    workspaceSelection,
  );
  if (workspaceAuthorityError) {
    return NextResponse.json(
      {
        error: workspaceAuthorityError.error,
        code: workspaceAuthorityError.code,
      },
      { status: workspaceAuthorityError.status },
    );
  }

  let runtimeSessionId = chatId;
  const hasPriorMessages = hasMessagesBeforePendingTurn(chat);

  let started;
  try {
    started = beginStreamingMessage(ownerUserId, chatId, {
      ...body,
      workspaceSelection: workspaceSelection ?? undefined,
    });
  } catch (error) {
    if (!(error instanceof ChatStreamProjectionError)) throw error;
    return NextResponse.json({ error: error.message, code: error.code }, { status: 409 });
  }
  if (!started) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const backendAbortController = new AbortController();
  const knownArtifactIds = new Set<string>();

  return proxyRunStream({
    backendResponse: async (emit) => {
      const runtime = await getStreamRuntime();
      const liveWorkspaceSelection = await verifyLiveWorkspaceSelection(
        workspaceSelection,
        runtime,
      );
      if (
        liveWorkspaceSelection &&
        !sameWorkspaceSelection(liveWorkspaceSelection, storedWorkspaceSelection)
      ) {
        const updated = await updateChatWorkspaceSelection(
          ownerUserId,
          chatId,
          liveWorkspaceSelection,
          auth.client,
        );
        if (!updated) {
          throw new Error("chat not found");
        }
      }
      const effectiveWorkspaceSelection =
        liveWorkspaceSelection ?? null;
      const workspaceBindings = resolveWorkspaceBindings(
        effectiveWorkspaceSelection,
      );
      const requestedModel = requireSelectedOfferingId(
        body.options?.model ?? chat.chat.model,
      );
      const [ensuredSessionId, modelSelection] = await Promise.all([
        ensureChatBackendSession(ownerUserId, chatId, {
          model: requestedModel,
          runtime,
        }),
        resolveModelOfferingSelection(runtime, requestedModel),
      ]);
      runtimeSessionId = ensuredSessionId;
      emit({
        type: "session_bound",
        chat_id: chatId,
        session_id: runtimeSessionId,
      });
      if (hasPriorMessages) {
        const existingArtifacts = await fetchSessionArtifacts(
          runtime,
          runtimeSessionId,
        );
        for (const artifact of existingArtifacts) {
          knownArtifactIds.add(artifact.id);
        }
      }
      return runtime.fetchResponse(PATH_CHAT_STREAM, {
        method: "POST",
        auth: "required",
        retryUnauthorized: true,
        operation: "stream web chat turn",
        signal: backendAbortController.signal,
        json: {
          message: body.content,
          parts: [],
          attachments: body.attachments ?? [],
          session_id: runtimeSessionId,
          model_selection: { offering_id: modelSelection.offeringId },
          allow_skills: activeSkills.length ? activeSkills : undefined,
          enabled_tools: activeTools,
          workspace_binding: workspaceBindings.workspaceBinding,
          executor_binding: workspaceBindings.executorBinding,
          context: {
            source: "web_v1",
            transport: "next_sse_proxy",
            edge_profile:
              activeSkills.length ||
              activeTools.length ||
              workspaceBindings.edgeProfile
                ? {
                    ...workspaceBindings.edgeProfile,
                    ...(activeSkills.length
                      ? { active_skills: activeSkills }
                      : {}),
                    ...(activeTools.length
                      ? { active_tools: activeTools }
                      : {}),
                  }
                : undefined,
            thinking: body.options?.thinking
              ? { mode: "adaptive", effort: "high" }
              : { mode: "off" },
          },
        },
      });
    },
    backendAbortController,
    ownerUserId,
    chatId,
    sessionId: () => runtimeSessionId,
    runtime: getStreamRuntime,
    assistantMessageId: started.assistantMessage.id,
    knownArtifactIds,
    localMessages: {
      userMessage: started.userMessage,
      assistantMessage: started.assistantMessage,
    },
  });
}

export async function GET(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  const auth = await requireRuntimeUser();
  if (auth.response) {
    return auth.response;
  }
  const ownerUserId = auth.user.user_id;
  const { chatId } = await context.params;
  const runId = request.nextUrl.searchParams.get("runId")?.trim();
  if (!runId) {
    return NextResponse.json({ error: "runId is required" }, { status: 400 });
  }

  const chat = getChat(ownerUserId, chatId);
  if (!chat) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }

  const replayOnly = request.nextUrl.searchParams.get("replay_only") === "true";
  const requestedAssistantMessageId = request.nextUrl.searchParams.get("assistantMessageId");
  const activeRunAssistantMessageId = chat.activeRun?.runId === runId
    ? chat.activeRun.assistantMessageId : null;
  const assistantMessageId = requestedAssistantMessageId !== null
    ? assistantMessageBelongsToChat(chat.messages, requestedAssistantMessageId.trim())
    : assistantMessageBelongsToChat(chat.messages, activeRunAssistantMessageId) ??
      (replayOnly ? lastAssistantMessageId(chat.messages) : null);
  if (!assistantMessageId) {
    const error = new ChatStreamProjectionError("stream_continuation_unavailable");
    return NextResponse.json({ error: error.message, code: error.code }, { status: 409 });
  }

  const runtime = auth.client;

  const sessionId = chat.session?.backendSessionId ?? chatId;
  const knownArtifactIds = new Set<string>();
  if (!replayOnly) {
    try {
      const existingArtifacts = await fetchSessionArtifacts(runtime, sessionId);
      for (const artifact of existingArtifacts) {
        knownArtifactIds.add(artifact.id);
      }
    } catch (error) {
      const message =
        error instanceof Error ? error.message : "Failed to load artifacts.";
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }

  const backendAbortController = new AbortController();
  const lastIndex = normalizedLastIndex(
    request.nextUrl.searchParams.get("last_index"),
  );
  let projectionLease: ChatStreamProjectionLease | undefined;
  if (!replayOnly) {
    try {
      projectionLease = claimChatStreamProjection(ownerUserId, chatId, assistantMessageId,
        backendAbortController, { sessionId, runId, nextEventIndex: lastIndex ?? 0 });
    } catch (error) {
      if (!(error instanceof ChatStreamProjectionError)) throw error;
      return NextResponse.json({ error: error.message, code: error.code }, { status: 409 });
    }
  }
  const resumeIndex = projectionLease?.projection.state.nextEventIndex ?? lastIndex;
  const backendStreamPath = `${chatRunStreamPath(runId)}${buildQueryString({
    ...(resumeIndex === null ? {} : { last_index: resumeIndex }),
    ...(replayOnly ? { replay_only: "true" } : {}),
  })}`;

  return proxyRunStream({
    backendResponse: () =>
      runtime.fetchResponse(backendStreamPath, {
        method: "GET",
        auth: "required",
        operation: `stream existing web run ${runId}`,
        signal: backendAbortController.signal,
      }),
    backendAbortController,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId,
    knownArtifactIds,
    replayOnly,
    replayFromOrigin: lastIndex === null || lastIndex === 0,
    projectionLease,
  });
}
