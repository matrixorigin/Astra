"use client";

import {
  AlertTriangle,
  Bot,
  ClipboardList,
  HardDrive,
  Loader2,
  MessageSquare,
  MoreVertical,
  Monitor,
  RefreshCw,
  Terminal,
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
import type {
  ChatDetail,
  EdgeStatusResponse,
  WorkspaceSelection,
} from "@/lib/api/types";
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

// ── helpers ──────────────────────────────────────────────────────────────────

function runActivityMetricCount(
  active: number,
  total: number,
): RunActivityMetricCount {
  if (active > 0) {
    return { active, total, value: active, mode: "active" };
  }
  return { active, total, value: total, mode: "item" };
}

type RunActivityMetricCount = {
  active: number;
  total: number;
  value: number;
  mode: "active" | "item";
};

// ── sub-components ──────────────────────────────────────────────────────────

function WorkspaceSelector({
  selection,
  explicit,
  edges,
  loading,
  error,
  disabled,
  onRefresh,
  onSelect,
}: {
  selection: WorkspaceSelection | null;
  explicit: boolean;
  edges: EdgeStatusResponse["edges"];
  loading: boolean;
  error: string | null;
  disabled: boolean;
  onRefresh: () => void | Promise<void>;
  onSelect: (selection: WorkspaceSelection) => void;
}) {
  const edgeOptions = edges
    .filter((edge) => edge.workspace_dir?.trim())
    .map((edge) => ({
      edgeAgentId: edge.edge_agent_id,
      displayName: edge.hostname ?? edge.edge_agent_id,
      cwd: edge.workspace_dir!,
      connectedSecs: edge.connected_secs,
    }));
  const selectedEdge =
    selection?.kind === "edge_workspace"
      ? edgeOptions.find(
          (edge) =>
            edge.edgeAgentId === selection.edgeAgentId &&
            edge.cwd === selection.cwd,
        )
      : null;
  const selectedEdgeMissing =
    explicit && selection?.kind === "edge_workspace" && !selectedEdge;
  const selectedOfflineLabel =
    selection?.kind === "edge_workspace"
      ? `${selection.displayName ?? selection.edgeAgentId} · ${selection.cwd}`
      : "";

  return (
    <div className="mb-2 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="font-medium text-text">Run in</span>
      {!explicit || !selection ? (
        <span className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1.5 font-medium text-text-secondary">
          <MessageSquare className="size-3.5 shrink-0 text-text-muted" />
          <span className="truncate">Astra</span>
        </span>
      ) : null}
      <button
        type="button"
        disabled={disabled}
        onClick={() => onSelect({ kind: "server_sandbox" })}
        className={[
          "inline-flex max-w-full items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
          explicit && selection?.kind === "server_sandbox"
            ? "bg-text text-white"
            : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
        ].join(" ")}
      >
        <HardDrive className="size-3.5 shrink-0" />
        <span className="truncate">Sandbox</span>
      </button>
      {edgeOptions.map((edge) => {
        const selected =
          explicit &&
          selection?.kind === "edge_workspace" &&
          selection.edgeAgentId === edge.edgeAgentId &&
          selection.cwd === edge.cwd;
        return (
          <button
            key={`${edge.edgeAgentId}:${edge.cwd}`}
            type="button"
            disabled={disabled}
            title={`${edge.displayName} · ${edge.cwd}`}
            onClick={() =>
              onSelect({
                kind: "edge_workspace",
                edgeAgentId: edge.edgeAgentId,
                displayName: edge.displayName,
                cwd: edge.cwd,
              })
            }
            className={[
              "inline-flex min-w-0 max-w-[min(30rem,100%)] items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
              selected
                ? "bg-text text-white"
                : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
            ].join(" ")}
          >
            <Monitor className="size-3.5 shrink-0" />
            <span className="truncate">{edge.displayName}</span>
            <span
              className={[
                "min-w-0 truncate border-l pl-1.5 font-normal",
                selected
                  ? "border-white/30 text-white/75"
                  : "border-border text-text-muted",
              ].join(" ")}
            >
              {edge.cwd}
            </span>
            <span className="sr-only">
              connected for {edge.connectedSecs} seconds
            </span>
          </button>
        );
      })}
      <button
        type="button"
        disabled={loading}
        onClick={() => {
          void onRefresh();
        }}
        className="inline-flex size-7 items-center justify-center rounded-full bg-bg text-text-muted transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-wait disabled:opacity-60"
        aria-label="Refresh environments"
        title="Refresh environments"
      >
        <RefreshCw
          className={["size-3.5", loading ? "animate-spin" : ""].join(" ")}
        />
      </button>
      {selectedEdgeMissing ? (
        <div
          className="flex min-w-0 max-w-full flex-wrap items-center gap-2 rounded-[10px] border border-warning/30 bg-warning/10 px-2.5 py-1.5 text-warning"
          role="status"
          aria-label={`Selected environment is unavailable: ${selectedOfflineLabel}`}
        >
          <AlertTriangle className="size-3.5 shrink-0" />
          <span className="min-w-0 max-w-[min(28rem,100%)] truncate">
            Environment unavailable · {selectedOfflineLabel}
          </span>
          <button
            type="button"
            disabled={disabled}
            onClick={() => onSelect({ kind: "server_sandbox" })}
            className="rounded-full bg-bg px-2 py-0.5 font-medium text-text transition hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
          >
            Use sandbox
          </button>
        </div>
      ) : null}
      {error ? (
        <span className="max-w-full truncate text-warning">{error}</span>
      ) : null}
    </div>
  );
}

function RunActivityBar({
  label,
  agents,
  tools,
  tasks,
  onOpenAgents,
  onOpenTasks,
  onOpenTools,
}: {
  label: string;
  agents: RunActivityMetricCount;
  tools: RunActivityMetricCount;
  tasks: RunActivityMetricCount;
  onOpenAgents: () => void;
  onOpenTasks: () => void;
  onOpenTools: () => void;
}) {
  return (
    <div className="mb-3 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="inline-flex min-w-0 items-center gap-2 font-medium text-text">
        <Loader2 className="size-3.5 animate-spin text-accent" />
        <span className="truncate">{label}</span>
      </span>
      <span
        className="inline-flex items-center gap-1 pl-0.5"
        aria-hidden="true"
      >
        <span className="size-1.5 animate-bounce rounded-full bg-text-muted" />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "120ms" }}
        />
        <span
          className="size-1.5 animate-bounce rounded-full bg-text-muted"
          style={{ animationDelay: "240ms" }}
        />
      </span>
      <span className="h-4 w-px bg-border" aria-hidden="true" />
      <RunActivityMetric
        icon={Bot}
        label="Agents"
        value={agents}
        onClick={onOpenAgents}
      />
      <RunActivityMetric
        icon={ClipboardList}
        label="Tasks"
        value={tasks}
        onClick={onOpenTasks}
      />
      <RunActivityMetric
        icon={Terminal}
        label="Tools"
        value={tools}
        onClick={onOpenTools}
      />
    </div>
  );
}

function RunActivityMetric({
  icon: Icon,
  label,
  value,
  onClick,
}: {
  icon: LucideIcon;
  label: string;
  value: RunActivityMetricCount;
  onClick: () => void;
}) {
  const unitLabel = value.mode === "active" ? "active" : "item";
  const pluralUnit =
    value.mode === "active" || value.value === 1 ? unitLabel : `${unitLabel}s`;
  const title =
    value.mode === "active"
      ? `Open ${label} (${value.active} active, ${value.total} total)`
      : `Open ${label} (${value.total} total)`;
  return (
    <button
      type="button"
      className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1 font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30"
      onClick={onClick}
      aria-label={`Open ${label.toLowerCase()} activity, ${value.value} ${pluralUnit}`}
      title={title}
    >
      <Icon className="size-3.5 text-text-muted" />
      <span className="tabular-nums text-text">{value.value}</span>
      <span className="hidden sm:inline">{label}</span>
      <span className="hidden text-text-muted xl:inline">{pluralUnit}</span>
    </button>
  );
}

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
  const agentActivityCount = runActivityMetricCount(
    workSurface.agents.filter((agent) =>
      ACTIVE_AGENT_SURFACE_STATUSES.has(agent.status),
    ).length,
    workSurface.agents.length,
  );
  const toolActivityCount = runActivityMetricCount(
    workSurface.tools.filter((tool) => tool.status === "running").length,
    workSurface.tools.length,
  );
  const openTaskCount = workSurface.tasks.filter((task) =>
    ["in_progress", "pending", "paused"].includes(task.status),
  ).length;
  const taskActivityCount = runActivityMetricCount(
    openTaskCount,
    workSurface.tasks.length,
  );
  const activeRunIsVisibleActivity = Boolean(
    detail.activeRun?.runId &&
    activeRunStatus &&
    !isTerminalChatRunStatus(activeRunStatus),
  );
  const workSurfaceActiveRun = activeRunIsVisibleActivity
    ? detail.activeRun
    : undefined;
  const showRunActivityBar = Boolean(
    !isArchived && !canResumeRun && (startingRun || activeRunIsVisibleActivity),
  );
  const runActivityLabel =
    stoppingRun || activeRunStatus === "cancelling"
      ? "Stopping"
      : startingRun
        ? "Thinking"
        : activeRunLabel;
  const workspaceSelectorDisabled = Boolean(
    startingRun ||
    composerDisabled ||
    (detail.activeRun?.runId &&
      activeRunStatus &&
      !isTerminalChatRunStatus(activeRunStatus)),
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
  const openWorkSurfaceTab = (tab: WorkSurfaceTab) => {
    setWorkSurfaceTab(tab);
    setWorkSurfaceOpenSignal((n) => n + 1);
  };

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
          onScroll={(event) => {
            const target = event.currentTarget;
            pinnedRef.current = isChatScrolledToBottom(target);
          }}
          className="min-h-0 flex-1 overscroll-contain overflow-y-auto scroll-smooth"
        >
          <div className="mx-auto w-full max-w-[860px] px-7 pb-44 pt-10">
            {detail.messages.map((message, index) => (
              <div key={message.id} data-chat-message-index={index}>
                <MessageBubble message={message} />
              </div>
            ))}
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
                {showRunActivityBar ? (
                  <RunActivityBar
                    label={runActivityLabel}
                    agents={agentActivityCount}
                    tools={toolActivityCount}
                    tasks={taskActivityCount}
                    onOpenAgents={() => openWorkSurfaceTab("agents")}
                    onOpenTasks={() => openWorkSurfaceTab("tasks")}
                    onOpenTools={() => openWorkSurfaceTab("tools")}
                  />
                ) : null}
                <WorkspaceSelector
                  selection={ws.workspaceSelection}
                  explicit={ws.workspaceSelectionExplicit}
                  edges={ws.edgeWorkspaces}
                  loading={ws.edgeWorkspacesLoading}
                  error={ws.edgeWorkspacesError}
                  disabled={workspaceSelectorDisabled}
                  onRefresh={ws.refreshEdgeWorkspaces}
                  onSelect={ws.setWorkspaceSelection}
                />
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
                  <div className="mt-3 flex items-center justify-between gap-3 rounded-[16px] border border-border/70 bg-surface px-4 py-3 text-sm text-text-muted">
                    <p>
                      {activeRunStatus === "paused"
                        ? "Astra is paused. Resume to continue or stop this run."
                        : activeRunBlocksNewInput
                          ? "Astra is busy. Stop it or wait before sending a new message."
                          : "Stopping..."}
                    </p>
                    <div className="flex shrink-0 items-center gap-2">
                      {canResumeRun ? (
                        <button
                          type="button"
                          onClick={() => {
                            void stream.resumeActiveRun();
                          }}
                          disabled={runControlBusy}
                          className="rounded-control bg-text px-3 py-2 text-sm font-medium text-white transition-colors hover:bg-text/90 disabled:cursor-not-allowed disabled:opacity-50"
                        >
                          {resumingRun ? "Resuming..." : "Resume"}
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
        openSignal={workSurfaceOpenSignal}
        onRefresh={() => {
          void stream.hydrateWorkSurfaceForChat();
        }}
        onLoadAgentRun={stream.loadAgentRunProjection}
      />
    </div>
  );
}
