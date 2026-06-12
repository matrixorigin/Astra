import { NextRequest, NextResponse } from "next/server";
import {
  PATH_CHAT_STREAM,
  PATH_EDGES_STATUS,
  chatRunStreamPath,
} from "@astra/sdk";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  ensureChatBackendSession,
  getChat,
  resolveBackendModelName,
  setChatActiveRun,
  updateChatWorkspaceSelection,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import { fetchSessionArtifacts } from "@/lib/api/stream-artifacts";
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
  EdgeStatusResponse,
  SendMessageRequest,
  WorkspaceSelection,
} from "@/lib/api/types";
import {
  normalizeSlashPath,
  normalizeWorkspaceSelection,
  resolveWorkspaceBindings,
  sameWorkspaceSelection,
  validateWorkspaceAuthority,
} from "@/lib/workspace-authority";

const encoder = new TextEncoder();

function sseFrame(event: unknown) {
  return encoder.encode(`data: ${JSON.stringify(event)}\n\n`);
}

function eventFromSseFrame(frame: string) {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim())
    .join("\n");

  if (!data || data === "[DONE]") {
    return null;
  }

  try {
    return JSON.parse(data) as Record<string, unknown>;
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

async function verifyLiveWorkspaceSelection(
  selection: WorkspaceSelection | null,
  runtime: WebRuntimeClient,
): Promise<
  | {
      selection: WorkspaceSelection | null;
      error: null;
    }
  | {
      selection: null;
      error: {
        code: string;
        message: string;
      };
    }
> {
  if (selection?.kind !== "edge_workspace") {
    return { selection, error: null };
  }

  const status = await runtime.get<EdgeStatusResponse>(PATH_EDGES_STATUS, {
    auth: "required",
    operation: "verify edge workspace binding",
  });
  const edge = status.edges.find(
    (candidate) => candidate.edge_agent_id === selection.edgeAgentId,
  );
  if (!edge) {
    return {
      selection: null,
      error: {
        code: "workspace_edge_offline",
        message: `Edge executor ${selection.displayName ?? selection.edgeAgentId} is offline. Reconnect edge or choose a connected workspace. Server fallback is disabled for this workspace.`,
      },
    };
  }

  const liveCwd = edge.workspace_dir?.trim() ?? "";
  if (
    !liveCwd ||
    normalizeSlashPath(liveCwd) !== normalizeSlashPath(selection.cwd)
  ) {
    const current = liveCwd
      ? `currently reports ${liveCwd}`
      : "does not report a workspace";
    return {
      selection: null,
      error: {
        code: "workspace_edge_path_unavailable",
        message: `Edge executor ${edge.hostname ?? selection.displayName ?? selection.edgeAgentId} ${current}, not ${selection.cwd}. Choose the current edge workspace, then retry. Server fallback is disabled for this workspace.`,
      },
    };
  }

  return {
    selection: {
      ...selection,
      displayName:
        edge.hostname ?? selection.displayName ?? selection.edgeAgentId,
      cwd: liveCwd,
    },
    error: null,
  };
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
  const recovered: SendMessageRequest = {
    content: chat.pendingTurn.content,
    options: chat.pendingTurn.options,
    pendingMessageId: chat.pendingTurn.messageId,
  };
  if (chat.workspaceSelection) {
    recovered.workspace = chat.workspaceSelection;
  }
  return recovered;
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
  knownArtifactIds: Set<string>;
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
    runtime,
    assistantMessageId,
    knownArtifactIds,
    localMessages,
  } = params;
  const currentSessionId =
    typeof sessionId === "function" ? sessionId : () => sessionId;
  const currentRuntime =
    typeof runtime === "function" ? runtime : () => Promise.resolve(runtime);

  let backendReader: ReadableStreamDefaultReader<Uint8Array> | null = null;
  let clientCancelled = false;

  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      if (localMessages) {
        controller.enqueue(
          sseFrame({
            type: "local_messages",
            user_message: localMessages.userMessage,
            assistant_message: localMessages.assistantMessage,
          }),
        );
      }

      void (async () => {
        let resolvedBackendResponse: Response;
        try {
          resolvedBackendResponse =
            typeof backendResponse === "function"
              ? await backendResponse((event) => {
                  controller.enqueue(sseFrame(event));
                })
              : backendResponse;
        } catch (error) {
          const message =
            error instanceof Error ? error.message : "Astra stream failed.";
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
          controller.enqueue(sseFrame({ type: "error", message }));
          controller.close();
          return;
        }
        if (!resolvedBackendResponse.ok || !resolvedBackendResponse.body) {
          const detail = await readErrorDetail(resolvedBackendResponse);
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
          controller.enqueue(sseFrame({ type: "error", message: detail }));
          controller.close();
          return;
        }

        const reader = resolvedBackendResponse.body.getReader();
        if (!reader) {
          controller.enqueue(
            sseFrame({
              type: "error",
              message: "Astra stream body is unavailable.",
            }),
          );
          controller.close();
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
            controller.enqueue(value);
            buffer += decoder.decode(value, { stream: true });

            const frames = buffer.split(/\r?\n\r?\n/);
            buffer = frames.pop() ?? "";
            for (const frame of frames) {
              const event = eventFromSseFrame(frame);
              if (event) {
                try {
                  applyStreamEvent(event, ctx, state);
                } catch (error) {
                  if (error instanceof Error) {
                    controller.enqueue(
                      sseFrame({ type: "error", message: error.message }),
                    );
                  }
                }
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
              try {
                applyStreamEvent(event, ctx, state);
              } catch (error) {
                if (error instanceof Error) {
                  controller.enqueue(
                    sseFrame({ type: "error", message: error.message }),
                  );
                }
              }
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
                  content: state.assistantText,
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

          if (state.lastStatus === "complete") {
            const runtimeClient = await currentRuntime();
            const artifacts = (
              await fetchSessionArtifacts(runtimeClient, currentSessionId())
            ).filter((artifact) => !knownArtifactIds.has(artifact.id));
            if (artifacts.length > 0) {
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  artifacts,
                },
              );
              controller.enqueue(sseFrame({ type: "artifacts", artifacts }));
            }
          }
        } catch (error) {
          if (clientCancelled) {
            return;
          }
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
          controller.enqueue(sseFrame({ type: "error", message }));
        } finally {
          backendReader = null;
          reader.releaseLock();
          if (!clientCancelled) {
            controller.close();
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
        error: "workspace must be a server sandbox or edge workspace selection",
        code: "invalid_workspace_selection",
      },
      { status: 400 },
    );
  }
  const workspaceSelection =
    requestedWorkspaceSelection ?? storedWorkspaceSelection;
  const workspaceError = validateWorkspaceAuthority(
    body.content,
    workspaceSelection,
  );
  if (workspaceError) {
    return NextResponse.json(
      { error: workspaceError.message, code: workspaceError.code },
      { status: 409 },
    );
  }
  let liveWorkspaceSelection = workspaceSelection;
  if (workspaceSelection?.kind === "edge_workspace") {
    try {
      const verified = await verifyLiveWorkspaceSelection(
        workspaceSelection,
        await getStreamRuntime(),
      );
      if (verified.error) {
        return NextResponse.json(
          { error: verified.error.message, code: verified.error.code },
          { status: 409 },
        );
      }
      liveWorkspaceSelection = verified.selection ?? undefined;
    } catch (error) {
      const status =
        error instanceof RuntimeClientError ? (error.status ?? 502) : 502;
      const message =
        error instanceof RuntimeClientError
          ? error.detail
          : error instanceof Error
            ? error.message
            : "Failed to verify edge workspace status.";
      return NextResponse.json(
        { error: message, code: "workspace_edge_status_unavailable" },
        { status },
      );
    }
  }
  if (
    liveWorkspaceSelection &&
    requestedWorkspaceSelection &&
    !sameWorkspaceSelection(liveWorkspaceSelection, storedWorkspaceSelection)
  ) {
    try {
      const updated = await updateChatWorkspaceSelection(
        ownerUserId,
        chatId,
        liveWorkspaceSelection,
      );
      if (!updated) {
        return NextResponse.json({ error: "chat not found" }, { status: 404 });
      }
    } catch (error) {
      const message =
        error instanceof Error
          ? error.message
          : "failed to persist workspace selection";
      return NextResponse.json({ error: message }, { status: 502 });
    }
  }
  const effectiveWorkspaceSelection =
    liveWorkspaceSelection ?? ({ kind: "server_sandbox" } as const);
  const workspaceBindings = resolveWorkspaceBindings(
    effectiveWorkspaceSelection,
  );
  let runtimeSessionId = chatId;
  const hasPriorMessages = hasMessagesBeforePendingTurn(chat);

  const started = beginStreamingMessage(ownerUserId, chatId, {
    ...body,
    workspaceSelection: liveWorkspaceSelection ?? undefined,
  });
  if (!started) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const backendAbortController = new AbortController();
  const knownArtifactIds = new Set<string>();

  return proxyRunStream({
    backendResponse: async (emit) => {
      const runtime = await getStreamRuntime();
      const [ensuredSessionId, model] = await Promise.all([
        ensureChatBackendSession(ownerUserId, chatId, {
          model: body.options?.model ?? chat.chat.model,
          runtime,
        }),
        resolveBackendModelName(runtime, body.options?.model),
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
        operation: "stream web chat turn",
        signal: backendAbortController.signal,
        json: {
          message: body.content,
          session_id: runtimeSessionId,
          model,
          allow_skills: activeSkills.length ? activeSkills : undefined,
          workspace_binding: workspaceBindings.workspaceBinding,
          executor_binding: workspaceBindings.executorBinding,
          context: {
            source: "web_v1",
            transport: "next_sse_proxy",
            edge_profile:
              activeSkills.length || workspaceBindings.edgeProfile
                ? {
                    ...workspaceBindings.edgeProfile,
                    ...(activeSkills.length
                      ? { active_skills: activeSkills }
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

  const assistantMessageId = lastAssistantMessageId(chat.messages);
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
  const knownArtifactIds = new Set<string>();
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

  const backendAbortController = new AbortController();
  const backendResponse = await runtime.fetchResponse(
    chatRunStreamPath(runId),
    {
      method: "GET",
      auth: "required",
      operation: `stream existing web run ${runId}`,
      signal: backendAbortController.signal,
    },
  );

  if (!backendResponse.ok || !backendResponse.body) {
    const detail = await readErrorDetail(backendResponse);
    updateStreamingAssistantMessage(ownerUserId, chatId, assistantMessageId, {
      content: detail,
      status: "failed",
    });
    return NextResponse.json(
      { error: detail },
      { status: backendResponse.status || 502 },
    );
  }

  return proxyRunStream({
    backendResponse,
    backendAbortController,
    ownerUserId,
    chatId,
    sessionId,
    runtime,
    assistantMessageId,
    knownArtifactIds,
  });
}
