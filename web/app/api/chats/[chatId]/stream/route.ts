import { NextRequest, NextResponse } from "next/server";
import {
  PATH_CHAT_STREAM,
  chatRunStreamPath,
  type RuntimeArtifactResponse,
} from "@astra/sdk";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import {
  beginStreamingMessage,
  ensureChatBackendSession,
  getChat,
  resolveBackendModelName,
  setChatActiveRun,
  updateStreamingAssistantMessage,
} from "@/lib/api/web-store";
import {
  WebRuntimeClient,
  readRuntimeErrorDetail,
  requireRuntimeClient,
} from "@/lib/runtime-client";
import type { ChatArtifactRef, SendMessageRequest } from "@/lib/api/types";

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
  } satisfies SendMessageRequest;
}

function stringField(value: unknown) {
  return typeof value === "string" && value.trim() ? value : null;
}

function numberField(value: unknown) {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

const INTERNAL_ARTIFACT_KINDS = new Set(["composite_snapshot_index"]);
const INTERNAL_ARTIFACT_SOURCES = new Set(["composite_snapshot_index"]);
const CHAT_VISIBLE_ARTIFACT_SOURCES = new Set(["publish_artifact"]);
const CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS = new Set(["artifact_file_v1"]);

function isChatVisibleRuntimeArtifact(
  source: string | null,
  kind: string,
  metadata: Record<string, unknown> | null,
) {
  if (
    INTERNAL_ARTIFACT_KINDS.has(kind) ||
    (source && INTERNAL_ARTIFACT_SOURCES.has(source))
  ) {
    return false;
  }

  const normalizeVersion = stringField(metadata?.normalize_version);
  return Boolean(
    source &&
    CHAT_VISIBLE_ARTIFACT_SOURCES.has(source) &&
    normalizeVersion &&
    CHAT_VISIBLE_ARTIFACT_NORMALIZE_VERSIONS.has(normalizeVersion),
  );
}

function artifactFromRuntime(
  artifact: RuntimeArtifactResponse,
): ChatArtifactRef | null {
  const content =
    artifact.content && typeof artifact.content === "object"
      ? (artifact.content as Record<string, unknown>)
      : null;
  const metadata =
    artifact.metadata && typeof artifact.metadata === "object"
      ? artifact.metadata
      : null;
  const id = stringField(artifact.artifact_id);
  const kind =
    stringField(artifact.artifact_kind) ?? stringField(content?.kind);
  const source = stringField(artifact.source);
  if (!id || !kind || !content) {
    return null;
  }
  if (!isChatVisibleRuntimeArtifact(source, kind, metadata)) {
    return null;
  }
  return {
    id,
    kind,
    source,
    title:
      stringField(content.title) ??
      stringField(metadata?.title) ??
      stringField(content.filename),
    filename:
      stringField(content.filename) ?? stringField(metadata?.download_filename),
    sizeBytes:
      numberField(content.byte_size) ?? numberField(metadata?.byte_size),
    contentType:
      stringField(content.content_type) ?? stringField(metadata?.content_type),
    renderer: stringField(content.renderer) ?? stringField(metadata?.renderer),
    downloadFilename: stringField(metadata?.download_filename),
    content,
    createdAt: artifact.created_at ?? null,
  };
}

async function fetchSessionArtifacts(
  client: WebRuntimeClient,
  sessionId: string,
) {
  const body = await client.sdk.listSessionArtifacts(sessionId, { limit: 50 });
  return (body.artifacts ?? [])
    .map(artifactFromRuntime)
    .filter((artifact): artifact is ChatArtifactRef => Boolean(artifact));
}

function mergeTextDelta(current: string, delta: string) {
  if (!delta || current === delta || current.endsWith(delta)) {
    return current;
  }
  if (delta.startsWith(current)) {
    return delta;
  }
  return `${current}${delta}`;
}

const THINKING_TAGS = [
  ["<thinking>", "</thinking>"],
  ["<think>", "</think>"],
] as const;

function splitThinkingTags(text: string) {
  const lower = text.toLowerCase();
  let cursor = 0;
  let visibleText = "";
  let reasoning = "";
  let hasThinking = false;
  let reasoningOpen = false;

  for (;;) {
    let match: { openIndex: number; openTag: string; closeTag: string } | null =
      null;
    for (const [openTag, closeTag] of THINKING_TAGS) {
      const openIndex = lower.indexOf(openTag, cursor);
      if (openIndex !== -1 && (!match || openIndex < match.openIndex)) {
        match = { openIndex, openTag, closeTag };
      }
    }

    if (!match) {
      let orphanClose: { closeIndex: number; closeTag: string } | null = null;
      for (const [, closeTag] of THINKING_TAGS) {
        const closeIndex = lower.indexOf(closeTag, cursor);
        if (
          closeIndex !== -1 &&
          (!orphanClose || closeIndex < orphanClose.closeIndex)
        ) {
          orphanClose = { closeIndex, closeTag };
        }
      }

      if (orphanClose) {
        hasThinking = true;
        reasoning += text.slice(cursor, orphanClose.closeIndex);
        cursor = orphanClose.closeIndex + orphanClose.closeTag.length;
        continue;
      }

      visibleText += text.slice(cursor);
      break;
    }

    hasThinking = true;
    visibleText += text.slice(cursor, match.openIndex);
    const reasoningStart = match.openIndex + match.openTag.length;
    const closeIndex = lower.indexOf(match.closeTag, reasoningStart);

    if (closeIndex === -1) {
      reasoning += text.slice(reasoningStart);
      reasoningOpen = true;
      break;
    }

    reasoning += text.slice(reasoningStart, closeIndex);
    cursor = closeIndex + match.closeTag.length;
  }

  return {
    visibleText: visibleText.replace(/\n{3,}/g, "\n\n").trim(),
    reasoning: reasoning.replace(/\n{3,}/g, "\n\n").trim(),
    hasThinking,
    reasoningOpen,
  };
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

  let assistantText = "";
  let assistantRawText = "";
  let reasoningText = "";
  let lastStatus: "streaming" | "complete" | "failed" = "streaming";
  let protocolError = false;
  let runLifecycle: "running" | "paused" | "finished" = "running";
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

        const applyEvent = (event: Record<string, unknown>) => {
          const type = typeof event.type === "string" ? event.type : "";
          const expectedSessionId = currentSessionId();
          if (protocolError) {
            return;
          }

          const applyAssistantText = (
            rawText: string,
            status: "streaming" | "complete" | "failed",
          ) => {
            const split = splitThinkingTags(rawText);
            assistantText = split.visibleText;
            if (split.hasThinking) {
              reasoningText = split.reasoning;
            }
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: assistantText,
                reasoning: reasoningText || undefined,
                reasoningStatus: reasoningText
                  ? split.reasoningOpen
                    ? "streaming"
                    : "complete"
                  : status === "streaming"
                    ? "streaming"
                    : status === "complete"
                      ? "complete"
                      : undefined,
                status,
              },
            );
          };

          if (type === "session_info" && typeof event.session_id === "string") {
            if (event.session_id !== expectedSessionId) {
              const message = `Runtime returned session_id ${event.session_id}, but Web chat is bound to ${expectedSessionId}.`;
              protocolError = true;
              assistantText = message;
              lastStatus = "failed";
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  content: message,
                  status: "failed",
                },
              );
              controller.enqueue(sseFrame({ type: "error", message }));
            }
            if (typeof event.run_id === "string") {
              setChatActiveRun(ownerUserId, chatId, {
                runId: event.run_id,
                status: "running",
                waitingFor: null,
              });
            }
            return;
          }

          if (type === "run_started" && typeof event.run_id === "string") {
            runLifecycle = "running";
            setChatActiveRun(ownerUserId, chatId, {
              runId: event.run_id,
              status: "running",
              waitingFor: null,
            });
            return;
          }

          if (type === "run_paused" && typeof event.run_id === "string") {
            runLifecycle = "paused";
            setChatActiveRun(ownerUserId, chatId, {
              runId: event.run_id,
              status: "paused",
              waitingFor: null,
            });
            return;
          }

          if (type === "run_resumed" && typeof event.run_id === "string") {
            runLifecycle = "running";
            setChatActiveRun(ownerUserId, chatId, {
              runId: event.run_id,
              status: "running",
              waitingFor: null,
            });
            return;
          }

          if (type === "text_delta" && typeof event.content === "string") {
            assistantRawText = mergeTextDelta(assistantRawText, event.content);
            applyAssistantText(assistantRawText, "streaming");
            return;
          }

          if (
            (type === "reasoning_delta" ||
              type === "thinking_delta" ||
              type === "reasoning_message_content") &&
            typeof event.content === "string"
          ) {
            reasoningText = mergeTextDelta(reasoningText, event.content);
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                reasoning: reasoningText,
                reasoningStatus: "streaming",
                status: "streaming",
              },
            );
            return;
          }

          if (type === "reasoning_done" || type === "thinking_done") {
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                reasoning: reasoningText,
                reasoningStatus: "complete",
                status: "streaming",
              },
            );
            return;
          }

          if (type === "text_done" && typeof event.full_text === "string") {
            assistantRawText = event.full_text;
            applyAssistantText(assistantRawText, "streaming");
            return;
          }

          if (
            type === "turn_complete" &&
            typeof event.assistant_text === "string"
          ) {
            assistantRawText = event.assistant_text;
            applyAssistantText(assistantRawText, lastStatus);
            return;
          }

          if (type === "error") {
            const message =
              typeof event.message === "string"
                ? event.message
                : "Astra stream failed.";
            assistantText = assistantText || message;
            lastStatus = "failed";
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: assistantText,
                status: "failed",
              },
            );
            return;
          }

          if (type === "run_finished") {
            const status =
              typeof event.status === "string" ? event.status : "completed";
            runLifecycle = "finished";
            setChatActiveRun(ownerUserId, chatId, undefined);
            if (status === "cancelled") {
              assistantText = assistantText || "Stopped.";
              lastStatus = "complete";
            } else if (status === "failed") {
              const message =
                typeof event.error === "string" ? event.error : assistantText;
              assistantText = message || assistantText;
              lastStatus = "failed";
            } else {
              lastStatus = "complete";
            }
            updateStreamingAssistantMessage(
              ownerUserId,
              chatId,
              assistantMessageId,
              {
                content: assistantText,
                reasoning: reasoningText || undefined,
                reasoningStatus:
                  lastStatus === "complete"
                    ? "complete"
                    : reasoningText
                      ? "complete"
                      : undefined,
                status: lastStatus,
              },
            );
          }
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
                applyEvent(event);
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
              applyEvent(event);
            }
          }

          if (clientCancelled) {
            return;
          }

          if (lastStatus === "streaming") {
            if (runLifecycle === "paused") {
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  content: assistantText,
                  reasoning: reasoningText || undefined,
                  reasoningStatus: reasoningText ? "streaming" : undefined,
                  status: "streaming",
                },
              );
            } else {
              lastStatus = assistantText ? "complete" : "failed";
              setChatActiveRun(ownerUserId, chatId, undefined);
              updateStreamingAssistantMessage(
                ownerUserId,
                chatId,
                assistantMessageId,
                {
                  content:
                    assistantText ||
                    "Astra completed the run without returning visible text.",
                  reasoning: reasoningText || undefined,
                  reasoningStatus:
                    lastStatus === "complete" ? "complete" : undefined,
                  status: lastStatus,
                },
              );
            }
          }

          if (lastStatus === "complete") {
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
              content: assistantText || message,
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
  let runtimeSessionId = chatId;
  const hasPriorMessages = hasMessagesBeforePendingTurn(chat);

  const started = beginStreamingMessage(ownerUserId, chatId, body);
  if (!started) {
    return NextResponse.json({ error: "chat not found" }, { status: 404 });
  }
  const backendAbortController = new AbortController();
  const knownArtifactIds = new Set<string>();
  let runtimePromise: Promise<WebRuntimeClient> | undefined;
  const getStreamRuntime = () => {
    runtimePromise ??= requireRuntimeClient({
      auth: "required",
      operation: "stream web chat turn",
    });
    return runtimePromise;
  };

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
          context: {
            source: "web_v1",
            transport: "next_sse_proxy",
            edge_profile: activeSkills.length
              ? { active_skills: activeSkills }
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
