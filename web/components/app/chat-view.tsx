"use client";

import {
  Activity,
  Bot,
  ClipboardList,
  MoreVertical,
  Wrench,
  type LucideIcon,
} from "lucide-react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useRef, useState } from "react";
import { ChatActionsMenu } from "@/components/app/chat-actions-menu";
import { ChatDotNavigator } from "@/components/app/chat-dot-navigator";
import { Composer } from "@/components/app/composer";
import { MessageBubble } from "@/components/app/message-bubble";
import { MoveChatModal } from "@/components/app/move-chat-modal";
import {
  WorkSurfacePanel,
  type WorkSurfaceTab,
} from "@/components/app/work-surface-panel";
import { IconButton } from "@/components/ui/icon-button";
import { useChatLifecycleActions } from "@/hooks/use-chat-lifecycle-actions";
import {
  useStreamLifecycle,
  canAttachRunStream,
} from "@/hooks/use-stream-lifecycle";
import { useWorkspaceSelection } from "@/hooks/use-workspace-selection";
import { subscribeChatLifecycleChange } from "@/lib/chat-lifecycle-events";
import { getChat } from "@/lib/api/chats";
import type { ChatDetail } from "@/lib/api/types";
import {
  deriveChatRunUiState,
  isTerminalChatRunStatus,
} from "@/lib/chat-run-state";
import {
  isChatScrolledToBottom,
  shouldAutoScrollChat,
} from "@/lib/chat-scroll-state";
import {
  ACTIVE_AGENT_SURFACE_STATUSES,
  createEmptyWorkSurface,
} from "@/lib/work-surface";
import { useToast } from "@/components/ui/toast";

// ── ChatView ─────────────────────────────────────────────────────────────────

export function ChatView({ initial }: { initial: ChatDetail }) {
  const router = useRouter();
  const [detail, setDetail] = useState(initial);
  const [moveOpen, setMoveOpen] = useState(false);

  // ── ui-pacing state (kept here so deriveChatRunUiState can depend on it) ──
  const [startingRun, setStartingRun] = useState(false);
  const [queueingDeferredInput, setQueueingDeferredInput] = useState(false);
  const [resumingRun, setResumingRun] = useState(false);
  const [stoppingRun, setStoppingRun] = useState(false);
  const [runAttachRetrySignal, setRunAttachRetrySignal] = useState(0);
  const [lastQueuedText, setLastQueuedText] = useState<string | undefined>();

  // ── work surface ───────────────────────────────────────────────────────────
  const [workSurface, setWorkSurface] = useState(() =>
    createEmptyWorkSurface(
      initial.session?.backendSessionId ?? null,
      initial.activeRun?.runId ?? null,
    ),
  );
  const [workSurfaceTab, setWorkSurfaceTab] = useState<WorkSurfaceTab>("tasks");
  const [workSurfaceOpenSignal, setWorkSurfaceOpenSignal] = useState(0);

  // ── scrolling ──────────────────────────────────────────────────────────────
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const pinnedRef = useRef(true);
  const touchStartYRef = useRef<number | null>(null);
  const pendingStartedRef = useRef<string | null>(null);

  // ── misc ───────────────────────────────────────────────────────────────────
  const lifecycle = useChatLifecycleActions({ onChatUpdated: setDetail });
  const { addToast } = useToast();

  // ── derived ────────────────────────────────────────────────────────────────
  const latestMessage = detail.messages[detail.messages.length - 1];
  const isArchived = Boolean(detail.chat.archivedAt);
  const chatListHref = detail.chat.projectId
    ? `/projects/${detail.chat.projectId}`
    : "/chats";

  // UI state derivation (must run before hooks that depend on it)
  const {
    activeRunStatus,
    canQueueDeferredInput,
    canResumeRun,
    canStopRun,
    activeRunBlocksNewInput,
    runControlBusy,
    composerDisabled,
    composerPlaceholder,
    taskBoardIntervention,
  } = deriveChatRunUiState({
    activeRun: detail.activeRun,
    archived: isArchived,
    startingRun,
    queueingDeferredInput,
    resumingRun,
    stoppingRun,
    lastQueuedText,
  });

  // ── hooks ──────────────────────────────────────────────────────────────────
  const ws = useWorkspaceSelection({ detail, setDetail });

  const stream = useStreamLifecycle({
    detail,
    setDetail,
    isArchived,
    chatListHref,
    workspaceSelection: ws.workspaceSelection,
    workspaceSelectionExplicit: ws.workspaceSelectionExplicit,
    canStopRun,
    canResumeRun,
    canQueueDeferredInput,
    setStartingRun,
    setQueueingDeferredInput,
    setResumingRun,
    setStoppingRun,
    setWorkSurface,
    runAttachRetrySignal,
    setRunAttachRetrySignal,
    refreshEdgeWorkspaces: ws.refreshEdgeWorkspaces,
  });
  const hydrateWorkSurfaceForChat = stream.hydrateWorkSurfaceForChat;
  const startStream = stream.startStream;

  // ── ui state derived from work surface ─────────────────────────────────────
  const showRunStatusPanel = Boolean(detail.activeRun?.runId && canResumeRun);
  const openTaskCount = workSurface.tasks.filter(
    (task) => !isTerminalTaskStatus(task.status),
  ).length;
  const activeAgentCount = workSurface.agents.filter((agent) =>
    ACTIVE_AGENT_SURFACE_STATUSES.has(agent.status.toLowerCase()),
  ).length;
  const agentAttentionCount = workSurface.agents.filter((agent) =>
    ["failed", "interrupted", "cancelled", "waiting"].includes(
      agent.status.toLowerCase(),
    ),
  ).length;
  const activeToolCount = workSurface.tools.filter(
    (tool) =>
      !tool.finishedAt &&
      !["success", "error", "cancelled"].includes(tool.status),
  ).length;
  const toolAttentionCount = workSurface.tools.filter(
    (tool) => tool.blocked || tool.status === "error",
  ).length;
  const openWorkSurface = (tab: WorkSurfaceTab) => {
    setWorkSurfaceTab(tab);
    setWorkSurfaceOpenSignal((value) => value + 1);
  };
  const activeRunIsVisibleActivity = Boolean(
    detail.activeRun?.runId &&
    activeRunStatus &&
    !isTerminalChatRunStatus(activeRunStatus),
  );
  const workSurfaceActiveRun = activeRunIsVisibleActivity
    ? detail.activeRun
    : undefined;
  const showConversationActivityStrip =
    workSurface.hydrated &&
    (activeRunIsVisibleActivity ||
      taskBoardIntervention ||
      openTaskCount > 0 ||
      agentAttentionCount > 0 ||
      toolAttentionCount > 0) &&
    (workSurface.tasks.length > 0 ||
      workSurface.agents.length > 0 ||
      workSurface.tools.length > 0);
  const conversationActivityKey = showConversationActivityStrip
    ? [
        openTaskCount,
        workSurface.agents.length,
        workSurface.tools.length,
        agentAttentionCount,
        toolAttentionCount,
      ].join(":")
    : "";

  // ── effects ────────────────────────────────────────────────────────────────

  // Clear last-queued text when the run moves past "input-queued" state.
  useEffect(() => {
    if (activeRunStatus !== "input-queued" && lastQueuedText) {
      setLastQueuedText(undefined);
    }
  }, [activeRunStatus, lastQueuedText]);

  // Cleanup on unmount
  useEffect(
    () => () => {
      stream.streamAbortRef.current?.abort();
      if (stream.autoAttachRetryTimerRef.current) {
        window.clearTimeout(stream.autoAttachRetryTimerRef.current);
        stream.autoAttachRetryTimerRef.current = undefined;
      }
      stream.stopReconcileRef.current();
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [],
  );

  // Hydrate work surface
  useEffect(() => {
    void hydrateWorkSurfaceForChat();
  }, [hydrateWorkSurfaceForChat]);

  // Auto-attach to an existing run stream
  useEffect(() => {
    const activeRun = detail.activeRun;
    if (!activeRun?.runId) {
      stream.autoAttachAttemptedRunRef.current = null;
      stream.autoAttachRetryCountsRef.current.clear();
      if (stream.autoAttachRetryTimerRef.current) {
        window.clearTimeout(stream.autoAttachRetryTimerRef.current);
        stream.autoAttachRetryTimerRef.current = undefined;
      }
      return;
    }
    if (
      isArchived ||
      detail.pendingTurn ||
      !canAttachRunStream(activeRun.status) ||
      stream.attachedRunRef.current === activeRun.runId ||
      stream.autoAttachAttemptedRunRef.current === activeRun.runId
    ) {
      return;
    }

    stream.autoAttachAttemptedRunRef.current = activeRun.runId;
    const assistantMessageId = stream.ensureStreamingAssistantMessage(
      activeRun.assistantMessageId,
    );
    stream.attachExistingRunStream(
      activeRun.runId,
      assistantMessageId,
      "The run is active, but the web UI could not reconnect to its stream.",
      {
        scheduleRetry: () => stream.scheduleAutoAttachRetry(activeRun.runId),
      },
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    detail.activeRun,
    detail.pendingTurn,
    isArchived,
    runAttachRetrySignal,
    // Stable callbacks that the effect logically depends on:
    stream.attachExistingRunStream,
    stream.ensureStreamingAssistantMessage,
    stream.scheduleAutoAttachRetry,
  ]);

  // Auto-scroll when messages change
  useEffect(() => {
    if (shouldAutoScrollChat({ pinnedToBottom: pinnedRef.current })) {
      scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
    }
  }, [
    detail.messages.length,
    latestMessage?.content,
    latestMessage?.reasoning,
    latestMessage?.artifacts?.length,
    conversationActivityKey,
  ]);

  // Process pending turn
  useEffect(() => {
    if (
      isArchived ||
      !detail.pendingTurn ||
      pendingStartedRef.current === detail.pendingTurn.messageId
    ) {
      return;
    }
    const pendingTurn = detail.pendingTurn;
    const timer = window.setTimeout(() => {
      if (pendingStartedRef.current === pendingTurn.messageId) {
        return;
      }
      pendingStartedRef.current = pendingTurn.messageId;
      void startStream({
        text: pendingTurn.content,
        options: pendingTurn.options,
        pendingMessageId: pendingTurn.messageId,
        appendUser: false,
      });
    }, 0);
    return () => window.clearTimeout(timer);
  }, [detail.pendingTurn, isArchived, startStream]);

  // Chat lifecycle subscription
  useEffect(
    () =>
      subscribeChatLifecycleChange((event) => {
        if (event.action === "clearArchived") {
          if (isArchived) {
            router.replace(chatListHref);
          }
          return;
        }
        if (event.chatId !== detail.chat.id) {
          return;
        }
        if (event.action === "delete") {
          router.replace(chatListHref);
          return;
        }
        if (event.chat) {
          setDetail(event.chat);
          return;
        }
        void getChat(detail.chat.id)
          .then(setDetail)
          .catch((error: unknown) => {
            addToast(
              error instanceof Error
                ? error.message
                : "Failed to refresh chat state.",
              "error",
            );
          });
      }),
    [addToast, chatListHref, detail.chat.id, isArchived, router],
  );

  // ── handlers kept at component level ───────────────────────────────────────
  // ── render ─────────────────────────────────────────────────────────────────
  return (
    <div className="astra-chat-view relative flex h-full min-h-0 overflow-hidden bg-bg">
      <div className="relative flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
        <header className="relative z-10 flex min-h-[58px] shrink-0 items-center gap-4 border-b border-border/60 bg-bg/85 px-7 backdrop-blur">
          <Link
            href={
              detail.chat.projectId
                ? `/projects/${detail.chat.projectId}`
                : "/chats"
            }
            className="inline-flex items-center gap-1 text-[13px] text-text-muted transition-colors hover:text-text"
          >
            ← {detail.project?.name ?? "Chats"}
          </Link>
          <div className="min-w-0">
            <h1 className="truncate text-sm font-semibold tracking-[-0.01em]">
              {detail.chat.title ?? "Untitled"}
            </h1>
            <div className="mt-0.5 flex items-center gap-1.5 text-xs text-text-muted">
              <span className="size-1.5 rounded-full bg-success" />
              <span>{detail.chat.model ?? "Default model"}</span>
            </div>
          </div>
          <div className="min-w-0 flex-1" />
          {detail.chat.archivedAt ? (
            <span className="rounded-full bg-surface-muted px-2 py-1 text-xs font-medium text-text-muted">
              Archived
            </span>
          ) : null}
          <ChatActionsMenu
            chatId={detail.chat.id}
            archived={isArchived}
            active
            afterMutationHref={chatListHref}
            onMove={() => setMoveOpen(true)}
            onChatUpdated={setDetail}
            trigger={
              <IconButton
                icon={MoreVertical}
                label="Chat menu"
                className="size-8"
              />
            }
          />
        </header>

        <div
          ref={scrollRef}
          data-testid="chat-scroll-container"
          onWheel={(event) => {
            if (event.deltaY < 0) {
              pinnedRef.current = false;
            }
          }}
          onTouchStart={(event) => {
            touchStartYRef.current = event.touches[0]?.clientY ?? null;
          }}
          onTouchMove={(event) => {
            const startY = touchStartYRef.current;
            const currentY = event.touches[0]?.clientY;
            if (
              startY !== null &&
              currentY !== undefined &&
              currentY > startY
            ) {
              pinnedRef.current = false;
            }
          }}
          onScroll={(event) => {
            const target = event.currentTarget;
            pinnedRef.current = isChatScrolledToBottom(target);
          }}
          className="min-h-0 flex-1 overscroll-contain overflow-y-auto"
        >
          <div className="mx-auto w-full max-w-[860px] px-7 pb-44 pt-10">
            {detail.messages.map((message, index) => (
              <div key={message.id} data-chat-message-index={index}>
                <MessageBubble message={message} />
              </div>
            ))}
            {showConversationActivityStrip ? (
              <ConversationActivityStrip
                taskCount={openTaskCount}
                agentCount={workSurface.agents.length}
                activeAgentCount={activeAgentCount}
                toolCount={workSurface.tools.length}
                activeToolCount={activeToolCount}
                agentAttentionCount={agentAttentionCount}
                toolAttentionCount={toolAttentionCount}
                taskBoardIntervention={taskBoardIntervention}
                active={activeRunIsVisibleActivity}
                onOpen={openWorkSurface}
              />
            ) : null}
          </div>
        </div>
        <ChatDotNavigator
          messages={detail.messages}
          scrollContainerRef={scrollRef}
        />

        <div className="pointer-events-none absolute inset-x-0 bottom-0 z-10 bg-gradient-to-t from-bg via-bg/95 to-bg/0 px-7 pb-6 pt-12">
          <div className="pointer-events-auto mx-auto w-full max-w-[860px]">
            {isArchived ? (
              <div className="rounded-[20px] border border-border bg-surface px-5 py-4 shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.06),0_0_0_0.5px_rgba(120,113,108,0.18)]">
                <div className="flex items-center justify-between gap-4">
                  <div className="min-w-0">
                    <p className="text-sm font-medium text-text">
                      This chat is archived.
                    </p>
                    <p className="mt-1 text-sm text-text-muted">
                      Archived chats are read-only. Unarchive it to continue.
                    </p>
                  </div>
                  <button
                    type="button"
                    disabled={lifecycle.busyChatId === detail.chat.id}
                    onClick={() => {
                      void lifecycle.unarchive(detail.chat.id);
                    }}
                    className="shrink-0 rounded-control bg-text px-3 py-2 text-sm font-medium text-white hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                  >
                    Unarchive
                  </button>
                </div>
              </div>
            ) : (
              <>
                <Composer
                  disabled={composerDisabled}
                  placeholder={composerPlaceholder}
                  initialModel={
                    detail.pendingTurn?.options.model ??
                    detail.chat.model ??
                    undefined
                  }
                  persistModelPreference={false}
                  onModelChange={stream.handleModelChange}
                  showStop={Boolean(canStopRun && canQueueDeferredInput)}
                  stopping={stoppingRun}
                  stopDisabled={runControlBusy}
                  onStop={() => {
                    void stream.stopActiveRun();
                  }}
                  projectContext={
                    detail.chat.projectId
                      ? { projectId: detail.chat.projectId }
                      : undefined
                  }
                  onSubmit={async ({ text, options }) => {
                    if (canQueueDeferredInput) {
                      try {
                        await stream.queueDeferredInput({ text, options });
                        setLastQueuedText(text);
                      } catch (error) {
                        // API failed — don't show "queued" in UI
                        console.error("Failed to queue deferred input:", error);
                      }
                      return;
                    }
                    void stream.startStream({
                      text,
                      options,
                      appendUser: true,
                    });
                  }}
                />
                {showRunStatusPanel ? (
                  <div className="mt-3 rounded-card border border-border/70 bg-surface px-4 py-3 text-sm text-text-muted">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <p className="font-medium text-text">
                          {taskBoardIntervention
                            ? "Task needs direction"
                            : activeRunStatus === "paused"
                              ? "Run paused"
                              : activeRunBlocksNewInput
                                ? "Run in progress"
                                : "Stopping"}
                        </p>
                        <p className="mt-1 leading-5">
                          {taskBoardIntervention
                            ? "Open the task board or continue the run when you are ready."
                            : activeRunStatus === "paused"
                              ? "Continue this run or close it before changing direction."
                              : activeRunBlocksNewInput
                                ? "Wait for the current run or stop it before sending a new message."
                                : "The stop request is being applied."}
                        </p>
                      </div>
                      <div className="flex shrink-0 items-center gap-2">
                        {taskBoardIntervention ? (
                          <button
                            type="button"
                            onClick={() => openWorkSurface("tasks")}
                            className="inline-flex items-center gap-1.5 rounded-control border border-border bg-bg px-3 py-2 text-sm font-medium text-text transition-colors hover:bg-surface-muted"
                          >
                            <ClipboardList className="size-4" />
                            Tasks
                          </button>
                        ) : null}
                        {agentAttentionCount > 0 ? (
                          <button
                            type="button"
                            onClick={() => openWorkSurface("agents")}
                            className="inline-flex items-center gap-1.5 rounded-control border border-border bg-bg px-3 py-2 text-sm font-medium text-text transition-colors hover:bg-surface-muted"
                          >
                            <Bot className="size-4" />
                            Agents
                          </button>
                        ) : null}
                        {canResumeRun ? (
                          <button
                            type="button"
                            onClick={() => {
                              void stream.resumeActiveRun();
                            }}
                            disabled={runControlBusy}
                            className="rounded-control bg-text px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                          >
                            {resumingRun
                              ? "Continuing..."
                              : taskBoardIntervention
                                ? "Continue"
                                : "Resume"}
                          </button>
                        ) : null}
                        {canStopRun ? (
                          <button
                            type="button"
                            onClick={() => {
                              void stream.stopActiveRun();
                            }}
                            disabled={runControlBusy}
                            className="rounded-control border border-border bg-bg px-3 py-2 text-sm font-medium text-text transition-colors hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
                          >
                            {stoppingRun ? "Stopping..." : "Stop"}
                          </button>
                        ) : null}
                      </div>
                    </div>
                  </div>
                ) : null}
              </>
            )}
          </div>
        </div>

        <MoveChatModal
          open={moveOpen}
          chatId={detail.chat.id}
          currentProjectId={detail.chat.projectId}
          onOpenChange={setMoveOpen}
          onMoved={stream.refresh}
        />
      </div>
      <WorkSurfacePanel
        state={workSurface}
        activeRun={workSurfaceActiveRun}
        tab={workSurfaceTab}
        onTabChange={setWorkSurfaceTab}
        onRefresh={() => {
          void stream.hydrateWorkSurfaceForChat();
        }}
        onLoadAgentRun={stream.loadAgentRunProjection}
        openSignal={workSurfaceOpenSignal}
      />
    </div>
  );
}

function ConversationActivityStrip({
  taskCount,
  agentCount,
  activeAgentCount,
  toolCount,
  activeToolCount,
  agentAttentionCount,
  toolAttentionCount,
  taskBoardIntervention,
  active,
  onOpen,
}: {
  taskCount: number;
  agentCount: number;
  activeAgentCount: number;
  toolCount: number;
  activeToolCount: number;
  agentAttentionCount: number;
  toolAttentionCount: number;
  taskBoardIntervention: boolean;
  active: boolean;
  onOpen: (tab: WorkSurfaceTab) => void;
}) {
  const needsAttention =
    taskBoardIntervention || agentAttentionCount > 0 || toolAttentionCount > 0;
  const title = taskBoardIntervention
    ? "Needs direction"
    : needsAttention
      ? "Needs attention"
      : active
        ? "Working"
        : "Activity";
  type ConversationActivityDescriptor = {
    tab: WorkSurfaceTab;
    icon: LucideIcon;
    label: string;
    count: number;
    activeCount?: number;
    attention?: number;
  };
  const actionCandidates: ConversationActivityDescriptor[] = [
    {
      tab: "tasks",
      icon: ClipboardList,
      label: "Tasks",
      count: taskCount,
      attention: taskBoardIntervention ? taskCount : 0,
    },
    {
      tab: "agents",
      icon: Bot,
      label: "Agents",
      count: agentCount,
      activeCount: activeAgentCount,
      attention: agentAttentionCount,
    },
    {
      tab: "tools",
      icon: Wrench,
      label: "Tools",
      count: toolCount,
      activeCount: activeToolCount,
      attention: toolAttentionCount,
    },
  ];
  const actions = actionCandidates.filter((action) => action.count > 0);
  const summary = actions
    .map((action) => {
      const activeCount = action.activeCount ?? action.count;
      const count = activeCount > 0 ? activeCount : action.count;
      return `${count} ${action.label.toLowerCase()}`;
    })
    .join(" · ");

  return (
    <div className="my-3 flex justify-center" aria-label="Background work">
      <div
        className={
          needsAttention
            ? "inline-flex max-w-full flex-wrap items-center gap-1.5 rounded-full border border-danger/20 bg-danger/5 px-2.5 py-1.5 text-xs shadow-sm"
            : "inline-flex max-w-full flex-wrap items-center gap-1.5 rounded-full border border-border/70 bg-surface/85 px-2.5 py-1.5 text-xs shadow-sm"
        }
      >
        <span
          className={
            needsAttention
              ? "inline-flex items-center gap-1.5 font-medium text-danger"
              : "inline-flex items-center gap-1.5 font-medium text-text-secondary"
          }
        >
          <Activity
            className={
              active && !needsAttention ? "size-3.5 animate-pulse" : "size-3.5"
            }
          />
          {title}
        </span>
        {summary ? (
          <span className="max-w-[18rem] truncate text-text-muted">
            {summary}
          </span>
        ) : null}
        {actions.length ? (
          <span className="inline-flex flex-wrap items-center gap-1">
            {actions.map((action) => (
              <ConversationActivityAction
                key={action.tab}
                {...action}
                onOpen={onOpen}
              />
            ))}
          </span>
        ) : null}
      </div>
    </div>
  );
}

function ConversationActivityAction({
  tab,
  icon: Icon,
  label,
  count,
  attention,
  onOpen,
}: {
  tab: WorkSurfaceTab;
  icon: LucideIcon;
  label: string;
  count: number;
  attention?: number;
  onOpen: (tab: WorkSurfaceTab) => void;
}) {
  const badge = attention && attention > 0 ? attention : count;
  const hasAttention = Boolean(attention && attention > 0);
  return (
    <button
      type="button"
      aria-label={`Open ${label.toLowerCase()} activity`}
      onClick={() => onOpen(tab)}
      className={
        hasAttention
          ? "inline-flex h-7 items-center gap-1 rounded-full border border-danger/20 bg-bg px-2 text-xs font-medium text-danger transition-colors hover:bg-danger/10"
          : "inline-flex h-7 items-center gap-1 rounded-full border border-border/70 bg-bg px-2 text-xs font-medium text-text-secondary transition-colors hover:bg-surface-muted"
      }
    >
      <Icon className="size-3.5" />
      <span>{label}</span>
      <span
        className={
          hasAttention
            ? "tabular-nums text-danger"
            : "tabular-nums text-text-muted"
        }
      >
        {badge}
      </span>
    </button>
  );
}

function isTerminalTaskStatus(status: string) {
  return [
    "completed",
    "complete",
    "done",
    "cancelled",
    "canceled",
    "failed",
    "skipped",
    "archived",
  ].includes(status.toLowerCase());
}
