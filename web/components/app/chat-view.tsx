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
import { getChat, getChatInsights } from "@/lib/api/chats";
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
  type AgentSurfaceItem,
  type SessionTask,
  type ToolSurfaceItem,
} from "@/lib/work-surface";
import { cn } from "@/lib/utils/cn";
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
    activeRunLabel,
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
  const showRunStatusPanel = Boolean(
    detail.activeRun?.runId &&
      (canResumeRun ||
        activeRunStatus === "blocked" ||
        activeRunStatus === "waiting"),
  );
  const openTaskCount = workSurface.tasks.filter(
    (task) => !isTerminalTaskStatus(task.status),
  ).length;
  const activeAgentCount = workSurface.agents.filter((agent) =>
    ACTIVE_AGENT_SURFACE_STATUSES.has(agent.status.toLowerCase()),
  ).length;
  const agentSignalCount = workSurface.agents.filter(
    agentNeedsConversationSignal,
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
      agentSignalCount > 0 ||
      toolAttentionCount > 0) &&
    (workSurface.tasks.length > 0 ||
      workSurface.agents.length > 0 ||
      workSurface.tools.length > 0);
  const conversationActivityKey = showConversationActivityStrip
    ? [
        openTaskCount,
        workSurface.agents.length,
        workSurface.tools.length,
        agentSignalCount,
        toolAttentionCount,
      ].join(":")
    : "";
  const headerRunState = chatHeaderRunState(
    stoppingRun ? "cancelling" : activeRunStatus,
  );

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
      <div className="astra-chat-main relative flex min-w-0 flex-1 flex-col overflow-hidden bg-bg">
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
              <span
                className={cn(
                  "size-1.5 rounded-full",
                  headerRunState.dotClass,
                  headerRunState.pulse && "animate-pulse",
                )}
              />
              <span>{headerRunState.label}</span>
              <span aria-hidden="true">·</span>
              <span className="truncate">
                {detail.chat.model ?? "Default model"}
              </span>
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
          data-chat-scroll-container="true"
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
              <ConversationWorkCard
                tasks={workSurface.tasks}
                agents={workSurface.agents}
                tools={workSurface.tools}
                activeToolCount={activeToolCount}
                agentSignalCount={agentSignalCount}
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
                  workspaceSelection={ws.workspaceSelection}
                  edgeWorkspaces={ws.edgeWorkspaces}
                  edgeWorkspacesLoading={ws.edgeWorkspacesLoading}
                  edgeWorkspacesError={ws.edgeWorkspacesError}
                  onWorkspaceSelectionChange={ws.setWorkspaceSelection}
                  onRefreshEdgeWorkspaces={() => {
                    void ws.refreshEdgeWorkspaces();
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
                            : activeRunStatus === "blocked" ||
                                activeRunStatus === "waiting"
                              ? activeRunLabel
                            : activeRunStatus === "paused"
                              ? "Run paused"
                              : activeRunBlocksNewInput
                                ? "Run in progress"
                                : "Stopping"}
                        </p>
                        <p className="mt-1 leading-5">
                          {taskBoardIntervention
                            ? "Open the task board or continue the run when you are ready."
                            : activeRunStatus === "blocked"
                              ? "Resolve the required environment or stop this run before changing direction."
                              : activeRunStatus === "waiting"
                                ? "Astra is waiting for an external action before continuing."
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
                        {agentSignalCount > 0 ? (
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
        defaultCollapsed
        onRefresh={() => {
          void stream.hydrateWorkSurfaceForChat();
        }}
        onLoadAgentRun={stream.loadAgentRunProjection}
        onLoadInsights={() => getChatInsights(detail.chat.id)}
        openSignal={workSurfaceOpenSignal}
      />
    </div>
  );
}

function chatHeaderRunState(status: string | null) {
  const normalized = status?.trim().toLowerCase();
  if (
    normalized &&
    ["running", "starting", "input-queued", "cancelling"].includes(normalized)
  ) {
    return {
      label:
        normalized === "input-queued"
          ? "Message queued"
          : normalized === "cancelling"
            ? "Stopping"
            : normalized === "starting"
              ? "Starting"
              : "Working",
      dotClass: "bg-accent",
      pulse: true,
    };
  }
  if (normalized === "waiting" || normalized === "paused") {
    return {
      label: normalized === "paused" ? "Paused" : "Waiting",
      dotClass: "bg-warning",
      pulse: false,
    };
  }
  if (normalized === "blocked" || normalized === "failed") {
    return {
      label: normalized === "blocked" ? "Needs attention" : "Run failed",
      dotClass: "bg-danger",
      pulse: false,
    };
  }
  if (normalized && isTerminalChatRunStatus(normalized)) {
    return { label: "Ready", dotClass: "bg-success", pulse: false };
  }
  if (normalized) {
    const label = statusLabel(normalized);
    return {
      label: label.charAt(0).toUpperCase() + label.slice(1),
      dotClass: "bg-accent",
      pulse: true,
    };
  }
  return { label: "Ready", dotClass: "bg-success", pulse: false };
}

function ConversationWorkCard({
  tasks,
  agents,
  tools,
  activeToolCount,
  agentSignalCount,
  toolAttentionCount,
  taskBoardIntervention,
  active,
  onOpen,
}: {
  tasks: SessionTask[];
  agents: AgentSurfaceItem[];
  tools: ToolSurfaceItem[];
  activeToolCount: number;
  agentSignalCount: number;
  toolAttentionCount: number;
  taskBoardIntervention: boolean;
  active: boolean;
  onOpen: (tab: WorkSurfaceTab) => void;
}) {
  const openTasks = tasks.filter((task) => !isTerminalTaskStatus(task.status));
  const activeAgents = agents.filter((agent) =>
    ACTIVE_AGENT_SURFACE_STATUSES.has(agent.status.toLowerCase()),
  );
  const signalAgents = agents.filter((agent) =>
    agentNeedsConversationSignal(agent),
  );
  const issueTools = tools.filter(
    (tool) => tool.blocked || tool.status === "error",
  );
  const activeTools = tools.filter((tool) => isActiveToolStatus(tool.status));
  const primaryTask = openTasks[0] ?? tasks[0];
  const primaryAgent = signalAgents[0] ?? activeAgents[0] ?? agents[0];
  const primaryTool = issueTools[0] ?? activeTools[0] ?? tools[0];
  const agentDangerCount = agents.filter(isDangerAgent).length;
  const agentWarningCount = agents.filter(isWarningAgent).length;
  const cardTone: ConversationTone =
    toolAttentionCount > 0 || agentDangerCount > 0
      ? "danger"
      : taskBoardIntervention || agentWarningCount > 0
        ? "warning"
        : active
          ? "running"
          : "neutral";
  const title = taskBoardIntervention
    ? "Needs direction"
    : cardTone === "danger"
      ? "Needs attention"
      : cardTone === "warning"
        ? "Waiting"
      : active
        ? "Working"
        : "Activity";
  type ConversationActivityDescriptor = {
    tab: WorkSurfaceTab;
    icon: LucideIcon;
    label: string;
    count: number;
    activeCount?: number;
    highlightCount?: number;
    tone?: ConversationTone;
  };
  const actionCandidates: ConversationActivityDescriptor[] = [
    {
      tab: "tasks",
      icon: ClipboardList,
      label: "Tasks",
      count: tasks.length,
      activeCount: openTasks.length,
      highlightCount: taskBoardIntervention
        ? openTasks.length || tasks.length
        : 0,
      tone: taskBoardIntervention ? "warning" : undefined,
    },
    {
      tab: "agents",
      icon: Bot,
      label: "Agents",
      count: agents.length,
      activeCount: activeAgents.length,
      highlightCount: agentSignalCount,
      tone:
        agentDangerCount > 0
          ? "danger"
          : agentWarningCount > 0
            ? "warning"
            : undefined,
    },
    {
      tab: "tools",
      icon: Wrench,
      label: "Tools",
      count: tools.length,
      activeCount: activeToolCount,
      highlightCount: toolAttentionCount,
      tone: toolAttentionCount > 0 ? "danger" : undefined,
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
  const rows = [
    primaryTask
      ? {
          key: "tasks",
          tab: "tasks" as WorkSurfaceTab,
          icon: ClipboardList,
          title: primaryTask.title,
          meta:
            taskBoardIntervention && openTasks.length > 1
              ? `${openTasks.length} open tasks`
              : statusLabel(primaryTask.status),
          status: taskBoardIntervention
            ? "Needs direction"
            : statusLabel(primaryTask.status),
          tone: taskBoardIntervention ? ("warning" as const) : undefined,
        }
      : null,
    primaryAgent
      ? {
          key: "agents",
          tab: "agents" as WorkSurfaceTab,
          icon: Bot,
          title:
            primaryAgent.description || primaryAgent.agentType || "Subagent",
          meta:
            activeAgents.length > 1
              ? `${activeAgents.length} active agents`
              : primaryAgent.agentType
                ? statusLabel(primaryAgent.agentType)
                : undefined,
          status: conversationStatusLabel(primaryAgent),
          tone: conversationAgentTone(primaryAgent),
        }
      : null,
    primaryTool
      ? {
          key: "tools",
          tab: "tools" as WorkSurfaceTab,
          icon: Wrench,
          title: primaryTool.tool,
          meta: toolMetaLabel(primaryTool, activeTools.length),
          status: toolStatusLabel(primaryTool),
          tone:
            primaryTool.blocked || primaryTool.status === "error"
              ? ("danger" as const)
              : undefined,
        }
      : null,
  ].filter((row): row is NonNullable<typeof row> => Boolean(row));

  return (
    <section className="my-3 flex justify-center" aria-label="Background work">
      <div
        className={`w-full max-w-[680px] rounded-card border p-3 shadow-sm ${conversationCardToneClass(cardTone)}`}
      >
        <div className="flex min-w-0 items-center justify-between gap-3">
          <div
            className={`inline-flex min-w-0 items-center gap-1.5 text-sm font-medium ${conversationHeadingToneClass(cardTone)}`}
          >
            <Activity
              className={
                active && cardTone === "running"
                  ? "size-4 shrink-0 animate-pulse"
                  : "size-4 shrink-0"
              }
            />
            <span className="shrink-0">{title}</span>
            {summary ? (
              <span className="truncate text-xs font-normal text-text-muted">
                {summary}
              </span>
            ) : null}
          </div>
          {actions.length ? (
            <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
              {actions.map((action) => (
                <ConversationActivityAction
                  key={action.tab}
                  {...action}
                  onOpen={onOpen}
                />
              ))}
            </div>
          ) : null}
        </div>
        {rows.length ? (
          <div className="mt-2 divide-y divide-border/60 overflow-hidden rounded-[8px] border border-border/60 bg-bg/70">
            {rows.map((row) => (
              <ConversationWorkRow
                key={row.key}
                icon={row.icon}
                title={row.title}
                meta={row.meta}
                status={row.status}
                tone={row.tone ?? "neutral"}
                onOpen={() => onOpen(row.tab)}
              />
            ))}
          </div>
        ) : null}
      </div>
    </section>
  );
}

function ConversationWorkRow({
  icon: Icon,
  title,
  meta,
  status,
  tone,
  onOpen,
}: {
  icon: LucideIcon;
  title: string;
  meta?: string;
  status: string;
  tone: ConversationTone;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onOpen}
      className="flex w-full min-w-0 items-center gap-2 px-2.5 py-2 text-left transition-colors hover:bg-surface-muted/70"
    >
      <Icon
        className={
          tone === "danger"
            ? "size-4 shrink-0 text-danger"
            : tone === "warning"
              ? "size-4 shrink-0 text-warning"
            : "size-4 shrink-0 text-accent"
        }
      />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm font-medium text-text">
          {title}
        </span>
        {meta ? (
          <span className="block truncate text-xs text-text-muted">{meta}</span>
        ) : null}
      </span>
      <span
        className={
          tone === "danger"
            ? "shrink-0 rounded-full bg-danger/10 px-2 py-0.5 text-[11px] font-medium text-danger"
            : tone === "warning"
              ? "shrink-0 rounded-full bg-warning/10 px-2 py-0.5 text-[11px] font-medium text-warning"
            : "shrink-0 rounded-full bg-surface-muted px-2 py-0.5 text-[11px] font-medium text-text-muted"
        }
      >
        {status}
      </span>
    </button>
  );
}

function ConversationActivityAction({
  tab,
  icon: Icon,
  label,
  count,
  highlightCount,
  tone,
  onOpen,
}: {
  tab: WorkSurfaceTab;
  icon: LucideIcon;
  label: string;
  count: number;
  highlightCount?: number;
  tone?: ConversationTone;
  onOpen: (tab: WorkSurfaceTab) => void;
}) {
  const badge = highlightCount && highlightCount > 0 ? highlightCount : count;
  const isHighlighted = Boolean(highlightCount && highlightCount > 0);
  const activeTone = isHighlighted ? (tone ?? "danger") : "neutral";
  return (
    <button
      type="button"
      aria-label={`Open ${label.toLowerCase()} activity`}
      onClick={() => onOpen(tab)}
      className={`inline-flex h-7 items-center gap-1 rounded-full border bg-bg px-2 text-xs font-medium transition-colors ${conversationActionToneClass(activeTone)}`}
    >
      <Icon className="size-3.5" />
      <span>{label}</span>
      <span
        className={
          activeTone === "danger"
            ? "tabular-nums text-danger"
            : activeTone === "warning"
              ? "tabular-nums text-warning"
            : "tabular-nums text-text-muted"
        }
      >
        {badge}
      </span>
    </button>
  );
}

type ConversationTone = "neutral" | "running" | "warning" | "danger";

function agentNeedsConversationSignal(agent: AgentSurfaceItem) {
  const status = agent.status.toLowerCase();
  return (
    status === "failed" ||
    status === "cancelled" ||
    status === "interrupted" ||
    status === "waiting" ||
    Boolean(agent.error)
  );
}

function isDangerAgent(agent: AgentSurfaceItem) {
  const normalized = agent.status.toLowerCase();
  if (normalized === "failed" || normalized === "cancelled") return true;
  if (normalized === "interrupted") return !isSoftAgentInterruption(agent);
  return Boolean(agent.error);
}

function isWarningAgent(agent: AgentSurfaceItem) {
  const normalized = agent.status.toLowerCase();
  if (normalized === "waiting") return true;
  if (normalized === "interrupted") return !isDangerAgent(agent);
  return false;
}

function isSoftAgentInterruption(agent: AgentSurfaceItem) {
  const reason = (agent.reason ?? "").trim().toLowerCase();
  return (
    !reason ||
    reason === "interrupted" ||
    reason === "empty_completion" ||
    reason === "budget_exhausted" ||
    reason === "turn_budget_exhausted" ||
    reason === "max_turns_exceeded" ||
    reason === "max_turns"
  );
}

function conversationAgentTone(agent: AgentSurfaceItem): ConversationTone {
  const normalized = agent.status.toLowerCase();
  if (isDangerAgent(agent)) return "danger";
  if (normalized === "waiting") return "warning";
  if (normalized === "interrupted") return "warning";
  return "neutral";
}

function conversationStatusLabel(agentOrStatus: AgentSurfaceItem | string) {
  if (typeof agentOrStatus === "string") {
    return agentOrStatus.toLowerCase() === "waiting"
      ? "Waiting"
      : statusLabel(agentOrStatus);
  }
  const status = agentOrStatus.status.toLowerCase();
  if (status === "waiting") return "Waiting";
  if (status === "interrupted") {
    const reason = (agentOrStatus.reason ?? "").trim().toLowerCase();
    if (
      !reason ||
      reason === "interrupted" ||
      reason === "empty_completion"
    ) {
      return "Needs final answer";
    }
    if (
      reason === "budget_exhausted" ||
      reason === "turn_budget_exhausted" ||
      reason === "max_turns_exceeded" ||
      reason === "max_turns"
    ) {
      return "Needs continuation";
    }
  }
  return statusLabel(agentOrStatus.status);
}

function conversationCardToneClass(tone: ConversationTone) {
  switch (tone) {
    case "danger":
      return "border-danger/20 bg-danger/5";
    case "warning":
      return "border-warning/25 bg-warning/10";
    case "running":
      return "border-border/70 bg-surface/85";
    case "neutral":
      return "border-border/70 bg-surface/85";
  }
}

function conversationHeadingToneClass(tone: ConversationTone) {
  switch (tone) {
    case "danger":
      return "text-danger";
    case "warning":
      return "text-warning";
    case "running":
      return "text-text-secondary";
    case "neutral":
      return "text-text-secondary";
  }
}

function conversationActionToneClass(tone: ConversationTone) {
  switch (tone) {
    case "danger":
      return "border-danger/20 text-danger hover:bg-danger/10";
    case "warning":
      return "border-warning/25 text-warning hover:bg-warning/10";
    case "running":
      return "border-border/70 text-text-secondary hover:bg-surface-muted";
    case "neutral":
      return "border-border/70 text-text-secondary hover:bg-surface-muted";
  }
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

function isActiveToolStatus(status: string) {
  return status.toLowerCase() === "running";
}

function toolStatusLabel(tool: ToolSurfaceItem) {
  if (tool.blocked) return "Needs attention";
  if (tool.status === "error") return "Failed";
  if (isActiveToolStatus(tool.status)) return "Running";
  return statusLabel(tool.status);
}

function toolMetaLabel(tool: ToolSurfaceItem, activeToolCount: number) {
  if (tool.errorKind) {
    return statusLabel(tool.errorKind);
  }
  if (activeToolCount > 1 && isActiveToolStatus(tool.status)) {
    return `${activeToolCount} active tools`;
  }
  if (typeof tool.durationMs === "number" && tool.durationMs > 0) {
    return formatDuration(tool.durationMs);
  }
  return statusLabel(tool.status);
}

function formatDuration(durationMs: number) {
  const seconds = Math.max(1, Math.round(durationMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return remainingSeconds > 0
    ? `${minutes}m ${remainingSeconds}s`
    : `${minutes}m`;
}

function statusLabel(status: string) {
  return status.replace(/[_-]+/g, " ");
}
