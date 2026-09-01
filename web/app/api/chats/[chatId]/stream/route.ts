import { NextRequest, NextResponse } from "next/server";
import {
  PATH_CHAT_STREAM,
  buildQueryString,
  chatRunStreamPath,
} from "@astra/sdk";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  ensureChatBackendSession,
  getChat,
  resolveModelOfferingSelection,
  requireSelectedOfferingId,
  setChatActiveRun,
  updateChatWorkspaceSelection,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import {
  applyStreamEvent,
  type StreamEventContext,
  type StreamEventState,
} from "@/lib/api/stream-event-handler";
import {
  RuntimeClientError,
  WebRuntimeClient,
  readRuntimeErrorDetail,
  requireRuntimeClient,
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

function eventFromSseFrame(
  frame: string,
): import("@astra/sdk").StreamEvent | null {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("\n");

  if (!data || data === "[DONE]") {
    return null;
  }

  try {
    return JSON.parse(data) as import("@astra/sdk").StreamEvent;
  } catch {
    return null;
  }
}

function normalizedActiveSkills(skills?: string[]) {
  if (!Array.isArray(skills)) {
    return [];
  }
  return [...new Set(skills.map((skill) => skill.trim()).filter(Boolean))].sort(
    (left, right) => left.localeCompare(right),
  );
}

function normalizedActiveTools(tools?: string[], webSearch = false) {
  const normalized = Array.isArray(tools)
    ? tools.map((tool) => tool.trim()).filter(Boolean)
    : [];
  if (webSearch) {
    normalized.push("web_search", "web_fetch");
  }
  return [...new Set(normalized)].sort((left, right) =>
    left.localeCompare(right),
  );
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
  assistantMessageId: string;
  localMessages?: {
    userMessage: unknown;
    assistantMessage: unknown;
  };
}) {
  const {
    backendResponse,
    backendAbortController,
    ownerUserId,
    chatId,
    sessionId,
    assistantMessageId,
    localMessages,
  } = params;
  const currentSessionId =
    typeof sessionId === "function" ? sessionId : () => sessionId;

  let backendReader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  let clientCancelled = false;

  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const enqueueFrame = (event: unknown) => {
        if (clientCancelled) {
          return false;
        }
        try {
          controller.enqueue(sseFrame(event));
          return true;
        } catch {
          clientCancelled = true;
          backendAbortController.abort();
          return false;
        }
      };
      const enqueueChunk = (chunk: Uint8Array) => {
        if (clientCancelled) {
          return false;
        }
        try {
          controller.enqueue(chunk);
          return true;
        } catch {
          clientCancelled = true;
          backendAbortController.abort();
          return false;
        }
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

      void (async () => {
        let resolvedBackendResponse: Response;
        try {
          resolvedBackendResponse =
            typeof backendResponse === "function"
              ? await backendResponse((event) => {
                  enqueueFrame(event);
                })
              : backendResponse;
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
          enqueueFrame({ type: "error", message, status, code });
          closeController();
          return;
        }
        if (clientCancelled) {
          return;
        }
        if (!resolvedBackendResponse.ok || !resolvedBackendResponse.body) {
          const detail = await readErrorDetail(resolvedBackendResponse);
          if (clientCancelled) {
            return;
          }
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
          enqueueFrame({ type: "error", message: detail });
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

        const state: StreamEventState = {
          assistantText: "",
          assistantRawText: "",
          reasoningText: "",
          lastStatus: "streaming",
          protocolError: false,
          runLifecycle: "running",
        };

        try {
          for (;;) {
            const { value, done } = await reader.read();
            if (clientCancelled) {
              return;
            }
            if (done) {
              break;
            }
            if (!enqueueChunk(value)) {
              return;
            }
            buffer += decoder.decode(value, { stream: true });

            const frames = buffer.split(/\r?\n\r?\n/);
            buffer = frames.pop() ?? "";
            for (const frame of frames) {
              const event = eventFromSseFrame(frame);
              if (event) {
                applyBackendStreamEvent(event, ctx, state);
              }
            }
          }

          const tail = decoder.decode();
          if (tail) {
            buffer += tail;
          }
          if (buffer.trim()) {
            const event = eventFromSseFrame(buffer);
            if (event) {
              applyBackendStreamEvent(event, ctx, state);
            }
          }

          if (clientCancelled) {
            return;
          }

          if (state.lastStatus === "streaming") {
            if (
              state.runLifecycle === "paused" ||
              state.runLifecycle === "blocked"
            ) {
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  content: state.assistantText || state.statusFeedbackText || "",
                  reasoning: state.reasoningText || undefined,
                  reasoningStatus: state.reasoningText
                    ? "streaming"
                    : undefined,
                  status: "streaming",
                },
              );
            } else {
              state.lastStatus = state.assistantText ? "complete" : "failed";
              setChatActiveRun(ownerUserId, chatId, undefined);
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  content:
                    state.assistantText ||
                    "Astra completed the run without returning visible text.",
                  reasoning: state.reasoningText || undefined,
                  reasoningStatus:
                    state.lastStatus === "complete" ? "complete" : undefined,
                  status: state.lastStatus,
                },
              );
            }
          }

        } catch (error) {
          if (clientCancelled) {
            return;
          }
          backendAbortController.abort();
          await Promise.resolve(reader.cancel()).catch(() => undefined);
          const message =
            error instanceof Error ? error.message : "Astra stream failed.";
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
          enqueueFrame({ type: "error", message });
        } finally {
          backendReader = null;
          reader.releaseLock();
          if (!clientCancelled) {
            closeController();
          }
        }
      })();
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
  let runtimePromise: Promise<WebRuntimeClient> | undefined;
  const getStreamRuntime = () => {
    runtimePromise ??= requireRuntimeClient({
      auth: "required",
      operation: "stream web chat turn",
    });
    return runtimePromise;
  };
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

  const started = beginStreamingMessage(ownerUserId, chatId, {
    ...body,
    workspaceSelection: workspaceSelection ?? undefined,
  });
  if (!started) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const backendAbortController = new AbortController();

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
      return runtime.fetchResponse(PATH_CHAT_STREAM, {
        method: "POST",
        auth: "required",
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
    assistantMessageId: started.assistantMessage.id,
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

  const requestedAssistantMessageId = request.nextUrl.searchParams
    .get("assistantMessageId")
    ?.trim();
  const activeRunAssistantMessageId =
    chat.activeRun?.runId === runId
      ? (chat.activeRun.assistantMessageId ?? null)
      : null;
  const assistantMessageId =
    assistantMessageBelongsToChat(chat.messages, requestedAssistantMessageId) ??
    assistantMessageBelongsToChat(chat.messages, activeRunAssistantMessageId) ??
    lastAssistantMessageId(chat.messages);
  if (!assistantMessageId) {
    return NextResponse.json(
      { error: "no assistant message is available to resume" },
      { status: 409 },
    );
  }

  let runtime: WebRuntimeClient;
  try {
    runtime = await requireRuntimeClient({
      auth: "required",
      operation: `stream existing web run ${runId}`,
    });
  } catch {
    return NextResponse.json({ error: "AUTH_REQUIRED" }, { status: 401 });
  }

  const sessionId = chat.session?.backendSessionId ?? chatId;
  const backendAbortController = new AbortController();
  const lastIndex = normalizedLastIndex(
    request.nextUrl.searchParams.get("last_index"),
  );
  const backendStreamPath = `${chatRunStreamPath(runId)}${buildQueryString(
    lastIndex === null ? {} : { last_index: lastIndex },
  )}`;

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
    assistantMessageId,
  });
}
