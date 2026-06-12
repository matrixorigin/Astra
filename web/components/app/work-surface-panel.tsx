"use client";

import {
  Activity,
  AlertTriangle,
  Bot,
  ChevronRight,
  CheckCircle2,
  Circle,
  ClipboardList,
  Loader2,
  Pause,
  RotateCw,
  Terminal,
  Wrench,
  X,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { ACTIVE_AGENT_SURFACE_STATUSES } from "@/lib/work-surface";
import type {
  AgentSurfaceItem,
  SessionTask,
  ToolSurfaceItem,
  WorkSurfaceState,
} from "@/lib/work-surface";
import type { WorkSurfaceRunResponse } from "@/lib/api/types";
import { cn } from "@/lib/utils/cn";

export type WorkSurfaceTab = "tasks" | "agents" | "tools";
type WorkSurfaceViewMode = "all" | "attention";

type WorkSurfacePanelProps = {
  state: WorkSurfaceState;
  activeRun?: { runId: string; status: string; waitingFor?: string | null };
  tab: WorkSurfaceTab;
  onTabChange: (tab: WorkSurfaceTab) => void;
  openSignal?: number;
  onRefresh: () => void;
  onLoadAgentRun: (runId: string) => Promise<WorkSurfaceRunResponse>;
};

type AgentRunProjectionState = {
  loading: boolean;
  error: string | null;
  projection: WorkSurfaceRunResponse | null;
};

type WorkspaceBindingLike =
  | {
      kind?: string | null;
      display_name?: string | null;
      cwd?: string | null;
      authority?: string | null;
      fallback_policy?: string | null;
    }
  | null
  | undefined;

type ExecutorBindingLike =
  | {
      kind?: string | null;
      executor_id?: string | null;
      display_name?: string | null;
      transport?: string | null;
      status?: string | null;
    }
  | null
  | undefined;

export function WorkSurfacePanel({
  state,
  activeRun,
  tab,
  onTabChange,
  openSignal,
  onRefresh,
  onLoadAgentRun,
}: WorkSurfacePanelProps) {
  const [collapsed, setCollapsed] = useState(false);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [viewModes, setViewModes] = useState<
    Record<WorkSurfaceTab, WorkSurfaceViewMode>
  >({
    tasks: "all",
    agents: "all",
    tools: "all",
  });
  const [agentRunDetails, setAgentRunDetails] = useState<
    Record<string, AgentRunProjectionState>
  >({});
  const openSignalRef = useRef(openSignal);
  const counts = useMemo(() => taskCounts(state.tasks), [state.tasks]);
  const attentionCounts = useMemo(
    () => ({
      tasks: state.tasks.filter(taskNeedsAttention).length,
      agents: state.agents.filter(agentNeedsAttention).length,
      tools: state.tools.filter(toolNeedsAttention).length,
    }),
    [state.agents, state.tasks, state.tools],
  );
  const taskViewMode =
    attentionCounts.tasks > 0 ? (viewModes.tasks ?? "all") : "all";
  const agentViewMode =
    attentionCounts.agents > 0 ? (viewModes.agents ?? "all") : "all";
  const toolViewMode =
    attentionCounts.tools > 0 ? (viewModes.tools ?? "all") : "all";
  const viewMode =
    tab === "tasks"
      ? taskViewMode
      : tab === "agents"
        ? agentViewMode
        : toolViewMode;
  const visibleTasks = useMemo(
    () =>
      taskViewMode === "attention"
        ? state.tasks.filter(taskNeedsAttention)
        : state.tasks,
    [state.tasks, taskViewMode],
  );
  const visibleAgents = useMemo(
    () =>
      agentViewMode === "attention"
        ? state.agents.filter(agentNeedsAttention)
        : state.agents,
    [state.agents, agentViewMode],
  );
  const visibleTools = useMemo(
    () =>
      toolViewMode === "attention"
        ? state.tools.filter(toolNeedsAttention)
        : state.tools,
    [state.tools, toolViewMode],
  );
  const visibleTaskCounts = useMemo(
    () => taskCounts(visibleTasks),
    [visibleTasks],
  );
  const agentActivityCount = state.agents.length;
  const toolActivityCount = state.tools.length;
  const visibleRunStatus = activeRun?.status ?? state.runStatus;
  const selectedAgent = selectedAgentId
    ? visibleAgents.find((agent) => agent.agentId === selectedAgentId)
    : undefined;

  useEffect(() => {
    if (tab !== "agents") {
      return;
    }
    if (!visibleAgents.length) {
      if (selectedAgentId) {
        setSelectedAgentId(null);
      }
      return;
    }
    if (
      selectedAgentId &&
      visibleAgents.some((agent) => agent.agentId === selectedAgentId)
    ) {
      return;
    }
    const latest = [...visibleAgents].sort(
      (left, right) => right.updatedAt - left.updatedAt,
    )[0];
    setSelectedAgentId(latest?.agentId ?? null);
  }, [selectedAgentId, visibleAgents, tab]);

  useEffect(() => {
    if (!selectedAgent?.runId) {
      return;
    }
    let cancelled = false;
    let timeout: number | undefined;
    let intervalMs = 2_500;
    const runId = selectedAgent.runId;
    const load = async (quiet = false) => {
      if (!quiet) {
        setAgentRunDetails((current) => ({
          ...current,
          [runId]: {
            loading: true,
            error: null,
            projection: current[runId]?.projection ?? null,
          },
        }));
      }
      try {
        const projection = await onLoadAgentRun(runId);
        if (cancelled) return;
        intervalMs = 2_500;
        setAgentRunDetails((current) => ({
          ...current,
          [runId]: {
            loading: false,
            error: null,
            projection,
          },
        }));
      } catch (error) {
        if (cancelled) return;
        intervalMs = Math.min(intervalMs * 2, 30_000);
        setAgentRunDetails((current) => ({
          ...current,
          [runId]: {
            loading: false,
            error:
              error instanceof Error
                ? error.message
                : "Failed to load subagent run.",
            projection: current[runId]?.projection ?? null,
          },
        }));
      }
    };

    const scheduleNext = () => {
      if (cancelled) return;
      if (document.hidden) {
        timeout = window.setTimeout(scheduleNext, intervalMs);
        return;
      }
      void load(true);
      timeout = window.setTimeout(scheduleNext, intervalMs);
    };

    void load();
    if (isAgentActive(selectedAgent.status)) {
      timeout = window.setTimeout(scheduleNext, intervalMs);
    }

    return () => {
      cancelled = true;
      if (timeout) {
        window.clearTimeout(timeout);
      }
    };
  }, [onLoadAgentRun, selectedAgent?.runId, selectedAgent?.status]);

  useEffect(() => {
    if (openSignal === undefined || openSignalRef.current === openSignal) {
      return;
    }
    openSignalRef.current = openSignal;
    setCollapsed(false);
    setMobileOpen(true);
    setViewModes((current) => ({
      ...current,
      [tab]: attentionCounts[tab] > 0 ? "attention" : "all",
    }));
  }, [attentionCounts, openSignal, tab]);

  useEffect(() => {
    if (viewModes[tab] !== "attention" || attentionCounts[tab] > 0) {
      return;
    }
    setViewModes((current) => ({ ...current, [tab]: "all" }));
  }, [attentionCounts, tab, viewModes]);

  const body = (
    <>
      <div className="flex h-14 shrink-0 items-center gap-2 border-b border-border/70 px-4">
        <button
          type="button"
          className="hidden size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text lg:inline-flex"
          onClick={() => setCollapsed((value) => !value)}
          aria-label={
            collapsed ? "Expand work surface" : "Collapse work surface"
          }
        >
          <ChevronRight
            className={cn("size-4 transition", collapsed ? "rotate-180" : "")}
          />
        </button>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <ClipboardList className="size-4 text-accent" />
            <h2 className="truncate text-sm font-semibold text-text">
              Work Surface
            </h2>
          </div>
          <p className="mt-0.5 truncate text-xs text-text-muted">
            {visibleRunStatus
              ? runStatusHeadline(visibleRunStatus)
              : state.hydrated
                ? "Session projection"
                : "Waiting for session"}
          </p>
        </div>
        <button
          type="button"
          className="inline-flex size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text"
          onClick={onRefresh}
          aria-label="Refresh work surface"
        >
          <RotateCw className={cn("size-4", state.loading && "animate-spin")} />
        </button>
        <button
          type="button"
          className="inline-flex size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text lg:hidden"
          onClick={() => setMobileOpen(false)}
          aria-label="Close work surface"
        >
          <X className="size-4" />
        </button>
      </div>

      <BindingStrip state={state} />
      <RunBlockedBanner blocked={state.blocked} />

      <div className="flex shrink-0 border-b border-border/70 px-2 py-2">
        <TabButton
          active={tab === "tasks"}
          icon={ClipboardList}
          label="Tasks"
          count={counts.open}
          onClick={() => onTabChange("tasks")}
        />
        <TabButton
          active={tab === "agents"}
          icon={Bot}
          label="Agents"
          count={agentActivityCount}
          onClick={() => onTabChange("agents")}
        />
        <TabButton
          active={tab === "tools"}
          icon={Terminal}
          label="Tools"
          count={toolActivityCount}
          onClick={() => onTabChange("tools")}
        />
      </div>

      {state.error ? (
        <div className="border-b border-border/70 px-4 py-3 text-xs text-danger">
          {state.error}
        </div>
      ) : null}
      {!state.error && state.warnings.length ? (
        <div className="border-b border-border/70 px-4 py-3 text-xs leading-5 text-warning">
          {state.warnings.join(" ")}
        </div>
      ) : null}
      <AttentionViewToggle
        tab={tab}
        mode={viewMode}
        attentionCount={attentionCounts[tab]}
        totalCount={tabItemCount(tab, state)}
        onModeChange={(mode) =>
          setViewModes((current) => ({ ...current, [tab]: mode }))
        }
      />

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        {tab === "tasks" ? (
          <TaskBoard
            tasks={visibleTasks}
            loading={state.loading}
            counts={visibleTaskCounts}
          />
        ) : null}
        {tab === "agents" ? (
          <AgentBoard
            agents={visibleAgents}
            loading={state.loading}
            selectedAgentId={selectedAgentId}
            agentRunDetails={agentRunDetails}
            onSelectAgent={(agentId) =>
              setSelectedAgentId((current) =>
                current === agentId ? null : agentId,
              )
            }
          />
        ) : null}
        {tab === "tools" ? (
          <ToolTimeline tools={visibleTools} loading={state.loading} />
        ) : null}
      </div>
    </>
  );

  return (
    <>
      <button
        type="button"
        className="fixed bottom-24 right-4 z-30 inline-flex items-center gap-2 rounded-full border border-border bg-surface px-3 py-2 text-sm font-medium text-text shadow-lg lg:hidden"
        onClick={() => setMobileOpen(true)}
      >
        <ClipboardList className="size-4" />
        Work
      </button>
      <aside
        className={cn(
          "hidden h-full shrink-0 border-l border-border/70 bg-surface/70 backdrop-blur lg:flex lg:flex-col",
          collapsed ? "w-[58px]" : "w-[360px] xl:w-[400px]",
        )}
      >
        {collapsed ? (
          <button
            type="button"
            className="flex h-full w-full flex-col items-center gap-3 py-4 text-text-muted transition hover:bg-surface-muted/60 hover:text-text"
            onClick={() => setCollapsed(false)}
            aria-label="Expand work surface"
          >
            <ClipboardList className="size-5" />
            <span className="[writing-mode:vertical-rl] text-xs font-medium">
              Work
            </span>
          </button>
        ) : (
          body
        )}
      </aside>
      {mobileOpen ? (
        <div className="fixed inset-0 z-40 lg:hidden">
          <button
            type="button"
            className="absolute inset-0 bg-black/20"
            aria-label="Close work surface"
            onClick={() => setMobileOpen(false)}
          />
          <aside className="absolute inset-y-0 right-0 flex w-[min(100vw,390px)] flex-col border-l border-border bg-surface shadow-2xl">
            {body}
          </aside>
        </div>
      ) : null}
    </>
  );
}

function TabButton({
  active,
  icon: Icon,
  label,
  count,
  onClick,
}: {
  active: boolean;
  icon: LucideIcon;
  label: string;
  count: number;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "inline-flex flex-1 items-center justify-center gap-1.5 rounded-control px-2.5 py-2 text-xs font-medium transition",
        active
          ? "bg-bg text-text shadow-sm"
          : "text-text-muted hover:bg-surface-muted hover:text-text",
      )}
    >
      <Icon className="size-3.5" />
      <span>{label}</span>
      {count ? (
        <span className="rounded-full bg-accent/10 px-1.5 text-[11px] text-accent">
          {count}
        </span>
      ) : null}
    </button>
  );
}

function AttentionViewToggle({
  tab,
  mode,
  attentionCount,
  totalCount,
  onModeChange,
}: {
  tab: WorkSurfaceTab;
  mode: WorkSurfaceViewMode;
  attentionCount: number;
  totalCount: number;
  onModeChange: (mode: WorkSurfaceViewMode) => void;
}) {
  if (!attentionCount) {
    return null;
  }
  const label = tab === "tasks" ? "items" : tab;
  return (
    <div className="border-b border-border/70 px-4 py-2">
      <div className="flex items-center gap-2 rounded-[8px] bg-bg p-1 text-xs">
        <button
          type="button"
          aria-pressed={mode === "attention"}
          onClick={() => onModeChange("attention")}
          className={cn(
            "inline-flex flex-1 items-center justify-center gap-1.5 rounded-[6px] px-2 py-1.5 font-medium transition",
            mode === "attention"
              ? "bg-danger/10 text-danger"
              : "text-text-muted hover:bg-surface-muted hover:text-text",
          )}
        >
          <AlertTriangle className="size-3.5" />
          <span>Needs attention</span>
          <span className="tabular-nums">{attentionCount}</span>
        </button>
        <button
          type="button"
          aria-pressed={mode === "all"}
          onClick={() => onModeChange("all")}
          className={cn(
            "inline-flex flex-1 items-center justify-center gap-1.5 rounded-[6px] px-2 py-1.5 font-medium transition",
            mode === "all"
              ? "bg-surface-muted text-text"
              : "text-text-muted hover:bg-surface-muted hover:text-text",
          )}
        >
          <span>All {label}</span>
          <span className="tabular-nums">{totalCount}</span>
        </button>
      </div>
    </div>
  );
}

function TaskBoard({
  tasks,
  loading,
  counts,
}: {
  tasks: SessionTask[];
  loading: boolean;
  counts: ReturnType<typeof taskCounts>;
}) {
  if (!tasks.length) {
    return <EmptySurface loading={loading} label="No tasks yet" />;
  }
  const sorted = [...tasks].sort(taskSort);
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-3 gap-2 text-center">
        <Metric label="Working" value={counts.working} />
        <Metric label="Queued" value={counts.queued} />
        <Metric label="Done" value={counts.done} />
      </div>
      <div className="space-y-2.5">
        {sorted.map((task) => (
          <TaskCard key={task.id} task={task} />
        ))}
      </div>
    </div>
  );
}

function TaskCard({ task }: { task: SessionTask }) {
  const subtasks = task.subtasks ?? [];
  const done = subtasks.filter((item) => isDone(item.status)).length;
  const progress = subtasks.length
    ? Math.round((done / subtasks.length) * 100)
    : isDone(task.status)
      ? 100
      : task.status === "in_progress"
        ? 45
        : 8;
  const running = task.status === "in_progress";
  return (
    <section className="rounded-card border border-border/70 bg-bg p-3 shadow-sm">
      <div className="flex items-start gap-3">
        <StatusIcon status={task.status} />
        <div className="min-w-0 flex-1 space-y-2">
          <div className="flex items-start gap-2">
            <div className="min-w-0 flex-1">
              <h3 className="line-clamp-2 text-sm font-semibold leading-5 text-text">
                {task.title}
              </h3>
              {task.active_form || task.description ? (
                <p className="mt-1 line-clamp-2 text-xs leading-5 text-text-muted">
                  {task.active_form ?? task.description}
                </p>
              ) : null}
            </div>
            <StatusPill status={task.status} active={running} />
          </div>
          <div className="h-1.5 overflow-hidden rounded-full bg-surface-muted">
            <div
              className={cn(
                "h-full rounded-full transition-all duration-500",
                isDone(task.status)
                  ? "bg-success"
                  : running
                    ? "bg-accent"
                    : "bg-border-strong",
              )}
              style={{ width: `${Math.max(5, progress)}%` }}
            />
          </div>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-text-muted">
            {subtasks.length ? (
              <span>
                {done}/{subtasks.length} subtasks
              </span>
            ) : (
              <span>{progress}%</span>
            )}
            {task.owner ? <span>Owner {task.owner}</span> : null}
            {task.blocked_by?.length ? (
              <span>{task.blocked_by.length} blockers</span>
            ) : null}
          </div>
          {subtasks.length ? (
            <div className="space-y-1">
              {subtasks.slice(0, 4).map((subtask) => (
                <div
                  key={subtask.id}
                  className="flex items-center gap-2 text-xs"
                >
                  <StatusIcon status={subtask.status} small />
                  <span
                    className={cn(
                      "min-w-0 flex-1 truncate",
                      isDone(subtask.status)
                        ? "text-text-muted line-through decoration-border-strong"
                        : "text-text-secondary",
                    )}
                  >
                    {subtask.title}
                  </span>
                </div>
              ))}
              {subtasks.length > 4 ? (
                <div className="pl-5 text-[11px] text-text-muted">
                  +{subtasks.length - 4} more
                </div>
              ) : null}
            </div>
          ) : null}
        </div>
      </div>
    </section>
  );
}

function AgentBoard({
  agents,
  loading,
  selectedAgentId,
  agentRunDetails,
  onSelectAgent,
}: {
  agents: AgentSurfaceItem[];
  loading: boolean;
  selectedAgentId: string | null;
  agentRunDetails: Record<string, AgentRunProjectionState>;
  onSelectAgent: (agentId: string) => void;
}) {
  if (!agents.length) {
    return <EmptySurface loading={loading} label="No subagent activity yet" />;
  }
  const sorted = [...agents].sort(
    (left, right) => right.updatedAt - left.updatedAt,
  );
  return (
    <div className="space-y-2.5">
      {sorted.map((agent) => (
        <AgentCard
          key={agent.agentId}
          agent={agent}
          selected={selectedAgentId === agent.agentId}
          runDetails={agent.runId ? agentRunDetails[agent.runId] : undefined}
          onSelect={() => onSelectAgent(agent.agentId)}
        />
      ))}
    </div>
  );
}

function AgentCard({
  agent,
  selected,
  runDetails,
  onSelect,
}: {
  agent: AgentSurfaceItem;
  selected: boolean;
  runDetails?: AgentRunProjectionState;
  onSelect: () => void;
}) {
  const active = isAgentActive(agent.status);
  const failed =
    agent.status === "failed" ||
    agent.status === "cancelled" ||
    agent.status === "interrupted";
  const progress =
    agent.turn && agent.maxTurns
      ? Math.min(100, Math.round((agent.turn / agent.maxTurns) * 100))
      : active
        ? 35
        : agent.status === "completed"
          ? 100
          : 0;
  const summary = agent.resultSummary ?? agent.error ?? agent.reason;
  const latestEvent = agent.events?.[agent.events.length - 1];
  return (
    <section
      className={cn(
        "rounded-card border bg-bg shadow-sm transition-colors",
        active
          ? "border-accent/35 bg-accent/5"
          : failed
            ? "border-danger/25 bg-danger/5"
            : "border-border/70",
      )}
    >
      <button
        type="button"
        className="flex w-full items-start gap-3 p-3 text-left"
        onClick={onSelect}
        aria-expanded={selected}
      >
        <div
          className={cn(
            "flex size-9 shrink-0 items-center justify-center rounded-control border",
            active
              ? "border-accent/25 bg-accent/10 text-accent"
              : failed
                ? "border-danger/20 bg-danger/10 text-danger"
                : "border-border bg-surface-muted text-text-muted",
          )}
        >
          {active ? (
            <Activity className="size-4 animate-pulse" />
          ) : (
            <Bot className="size-4" />
          )}
        </div>
        <div className="min-w-0 flex-1 space-y-2">
          <div className="flex items-start gap-2">
            <div className="min-w-0 flex-1">
              <h3 className="truncate text-sm font-semibold text-text">
                {agent.agentType ?? agent.agentId}
              </h3>
              <p className="mt-0.5 truncate font-mono text-[11px] text-text-muted">
                {agent.agentId}
              </p>
            </div>
            <StatusPill status={agent.status} active={active} />
          </div>
          {agent.description ? (
            <p className="line-clamp-2 text-xs leading-5 text-text-secondary">
              {agent.description}
            </p>
          ) : null}
          <AgentExecutionMeta agent={agent} />
          <div className="h-1.5 overflow-hidden rounded-full bg-surface-muted">
            <div
              className={cn(
                "h-full rounded-full transition-all duration-500",
                failed
                  ? "bg-danger"
                  : agent.status === "completed"
                    ? "bg-success"
                    : "bg-accent",
              )}
              style={{ width: `${Math.max(active ? 8 : 0, progress)}%` }}
            />
          </div>
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-text-muted">
            {latestEvent ? (
              <span className="inline-flex min-w-0 max-w-full items-center gap-1 text-accent">
                {active ? <MiniLiveDots /> : null}
                <span className="truncate">{latestEvent.label}</span>
              </span>
            ) : null}
            {agent.turn ? (
              <span>
                Turn {agent.turn}
                {agent.maxTurns ? `/${agent.maxTurns}` : ""}
              </span>
            ) : null}
            {agent.toolName ? (
              <span className="inline-flex items-center gap-1">
                <Wrench className="size-3" />
                {agent.toolName}
              </span>
            ) : null}
            {agent.totalToolCalls ? (
              <span>{agent.totalToolCalls} tools</span>
            ) : null}
            {agent.totalPromptTokens || agent.totalCompletionTokens ? (
              <span>
                {(agent.totalPromptTokens ?? 0) +
                  (agent.totalCompletionTokens ?? 0)}{" "}
                tokens
              </span>
            ) : null}
            {agent.durationMs ? (
              <span>{formatDuration(agent.durationMs)}</span>
            ) : null}
          </div>
          {summary ? (
            <p
              className={cn(
                "line-clamp-3 text-xs leading-5",
                failed ? "text-danger" : "text-text-muted",
              )}
            >
              {summary}
            </p>
          ) : active ? (
            <p className="text-xs leading-5 text-text-muted">
              {agent.toolName
                ? `Running ${agent.toolName}`
                : "Waiting for subagent progress"}
            </p>
          ) : null}
        </div>
      </button>
      {selected ? (
        <AgentDetails agent={agent} failed={failed} runDetails={runDetails} />
      ) : null}
    </section>
  );
}

function AgentDetails({
  agent,
  failed,
  runDetails,
}: {
  agent: AgentSurfaceItem;
  failed: boolean;
  runDetails?: AgentRunProjectionState;
}) {
  const updated = new Date(agent.updatedAt);
  const active = isAgentActive(agent.status);
  const ids = [
    ["Agent", agent.agentId],
    ["Run", agent.runId],
    ["Parent", agent.parentRunId],
    [
      "Workspace",
      agent.workspace
        ? (agent.workspace.cwd ?? workspaceDisplayName(agent.workspace))
        : undefined,
    ],
    [
      "Executor",
      agent.executor ? executorDisplayName(agent.executor) : undefined,
    ],
  ].filter((entry): entry is [string, string] => Boolean(entry[1]));
  return (
    <div className="border-t border-border/60 px-3 pb-3 pt-2">
      <AgentLiveEvents events={agent.events ?? []} active={active} />
      {agent.runId ? (
        <AgentRunProjection details={runDetails} active={active} />
      ) : null}
      {ids.length ? (
        <div className="mt-3 space-y-1.5">
          {ids.map(([label, value]) => (
            <div
              key={label}
              className="flex min-w-0 items-center gap-2 rounded-[6px] bg-surface-muted px-2 py-1.5"
            >
              <span className="w-12 shrink-0 text-[11px] font-medium text-text-muted">
                {label}
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-text">
                {value}
              </span>
            </div>
          ))}
        </div>
      ) : null}
      <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px] text-text-muted">
        <span>
          Updated{" "}
          {Number.isNaN(updated.getTime())
            ? "now"
            : updated.toLocaleTimeString()}
        </span>
        {agent.reason ? <span>Reason {statusLabel(agent.reason)}</span> : null}
        {agent.toolName ? <span>Tool {agent.toolName}</span> : null}
      </div>
      {agent.resultSummary || agent.error ? (
        <div
          className={cn(
            "mt-2 rounded-[6px] border px-2 py-1.5 text-xs leading-5",
            failed
              ? "border-danger/20 bg-danger/5 text-danger"
              : "border-border/70 bg-bg text-text-secondary",
          )}
        >
          {agent.error ?? agent.resultSummary}
        </div>
      ) : null}
    </div>
  );
}

function AgentExecutionMeta({ agent }: { agent: AgentSurfaceItem }) {
  const items = [
    agent.executor ? executorDisplayName(agent.executor) : undefined,
    agent.workspace?.cwd,
    agent.transport ? `transport ${statusLabel(agent.transport)}` : undefined,
    agent.fallbackPolicy
      ? `fallback ${statusLabel(agent.fallbackPolicy)}`
      : undefined,
  ].filter((item): item is string => Boolean(item));
  if (!items.length) {
    return null;
  }
  return (
    <div className="flex flex-wrap gap-x-2 gap-y-1 text-[11px] text-text-muted">
      {items.map((item) => (
        <span key={item} className="min-w-0 max-w-full truncate">
          {item}
        </span>
      ))}
    </div>
  );
}

function AgentLiveEvents({
  events,
  active,
}: {
  events: NonNullable<AgentSurfaceItem["events"]>;
  active: boolean;
}) {
  const recent = [...events].slice(-8).reverse();
  const transcript = agentLiveTranscript(events);
  return (
    <div className="rounded-[8px] border border-border/60 bg-surface/50 p-2.5">
      <div className="mb-2 flex items-center justify-between gap-3">
        <div className="inline-flex min-w-0 items-center gap-1.5 text-[11px] font-semibold text-text">
          <Activity className="size-3.5 text-accent" />
          <span>Live activity</span>
        </div>
        {active ? <MiniLiveDots className="shrink-0" /> : null}
      </div>
      {transcript ? (
        <div className="mb-2 rounded-[7px] border border-border/60 bg-bg px-2.5 py-2">
          <div className="mb-1 text-[10px] font-semibold uppercase text-text-muted">
            {transcript.label}
          </div>
          <div className="max-h-40 overflow-y-auto whitespace-pre-wrap break-words font-mono text-[11px] leading-5 text-text">
            {transcript.detail}
          </div>
        </div>
      ) : null}
      {recent.length ? (
        <div className="space-y-2">
          {recent.map((event) => (
            <AgentEventRow key={event.id} event={event} />
          ))}
        </div>
      ) : (
        <div className="flex items-center gap-2 rounded-[6px] bg-bg/70 px-2 py-2 text-xs text-text-muted">
          {active ? <MiniLiveDots /> : null}
          <span>Waiting for subagent activity</span>
        </div>
      )}
    </div>
  );
}

function agentLiveTranscript(events: NonNullable<AgentSurfaceItem["events"]>) {
  const textEvents = events.filter(
    (event) =>
      event.detail &&
      (event.type === "agent_live_event:output_delta" ||
        event.type === "agent_live_event:thinking_delta"),
  );
  if (!textEvents.length) {
    return null;
  }
  const latest = textEvents[textEvents.length - 1];
  const label =
    latest.type === "agent_live_event:thinking_delta"
      ? "Thinking preview"
      : "Live output";
  const detail = textEvents
    .map((event) => event.detail)
    .filter((detail): detail is string => Boolean(detail))
    .join("\n")
    .slice(-4000);
  return { label, detail };
}

function AgentRunProjection({
  details,
  active,
}: {
  details?: AgentRunProjectionState;
  active: boolean;
}) {
  const projection = details?.projection;
  const recent = [...(projection?.events ?? [])].slice(-8).reverse();
  return (
    <div className="mt-3 rounded-[8px] border border-border/60 bg-bg p-2.5">
      <div className="mb-2 flex items-center justify-between gap-3">
        <div className="inline-flex min-w-0 items-center gap-1.5 text-[11px] font-semibold text-text">
          <Terminal className="size-3.5 text-accent" />
          <span>Child run events</span>
        </div>
        {details?.loading || active ? (
          <MiniLiveDots className="shrink-0" />
        ) : null}
      </div>
      {projection ? <AgentRunBindingSummary projection={projection} /> : null}
      {details?.error ? (
        <div className="rounded-[6px] bg-danger/5 px-2 py-2 text-xs text-danger">
          {details.error}
        </div>
      ) : details?.loading && !projection ? (
        <div className="flex items-center gap-2 rounded-[6px] bg-surface-muted px-2 py-2 text-xs text-text-muted">
          <Loader2 className="size-3.5 animate-spin" />
          <span>Loading child run</span>
        </div>
      ) : recent.length ? (
        <div className="space-y-2">
          {recent.map((event, index) => (
            <RunProjectionEventRow
              key={`${eventType(event)}:${index}`}
              event={event}
            />
          ))}
        </div>
      ) : (
        <div className="rounded-[6px] bg-surface-muted px-2 py-2 text-xs text-text-muted">
          No child run events yet
        </div>
      )}
    </div>
  );
}

function AgentRunBindingSummary({
  projection,
}: {
  projection: WorkSurfaceRunResponse;
}) {
  // Extracting binding information with null guards for every field
  // so that downstream display fns never crash on unexpected shapes.
  const rawWs = projection.workspace;
  const rawEx = projection.executor;
  const workspace: WorkspaceBindingLike =
    rawWs && typeof rawWs === "object" && !Array.isArray(rawWs) ? rawWs : null;
  const executor: ExecutorBindingLike =
    rawEx && typeof rawEx === "object" && !Array.isArray(rawEx) ? rawEx : null;
  if (
    !workspace &&
    !executor &&
    !projection.transport &&
    !projection.fallbackPolicy
  ) {
    return null;
  }
  const items = [
    executor
      ? {
          label: "Executor",
          value: executorDisplayName(executor),
          detail: executorDetail(executor),
        }
      : null,
    workspace
      ? {
          label: "Workspace",
          value: workspaceDisplayName(workspace),
          detail: workspace.cwd ?? workspace.authority ?? undefined,
        }
      : null,
    projection.transport
      ? { label: "Transport", value: statusLabel(projection.transport) }
      : null,
    projection.fallbackPolicy
      ? { label: "Fallback", value: statusLabel(projection.fallbackPolicy) }
      : null,
  ].filter(Boolean) as Array<{
    label: string;
    value: string;
    detail?: string;
  }>;

  return (
    <div className="mb-2 grid gap-1.5 rounded-[6px] bg-surface-muted px-2 py-2 text-[11px] text-text-muted sm:grid-cols-2">
      {items.map((item) => (
        <div key={item.label} className="min-w-0">
          <span className="font-medium text-text-secondary">{item.label}</span>
          <span className="ml-1 text-text">{item.value}</span>
          {item.detail ? (
            <span className="ml-1 break-all text-text-muted">
              {item.detail}
            </span>
          ) : null}
        </div>
      ))}
    </div>
  );
}

function RunProjectionEventRow({ event }: { event: Record<string, unknown> }) {
  const type = eventType(event);
  const summary = describeRunProjectionEvent(event, type);
  return (
    <div className="grid grid-cols-[12px_1fr] items-start gap-2">
      <span
        className={cn(
          "mt-1.5 size-2 rounded-full",
          type === "run_finished"
            ? "bg-success"
            : type === "run_error"
              ? "bg-danger"
              : type.includes("tool")
                ? "bg-accent"
                : "bg-border-strong",
        )}
      />
      <div className="min-w-0">
        <div className="truncate text-xs font-medium text-text">
          {summary.label}
        </div>
        {summary.detail ? (
          <div className="mt-0.5 line-clamp-3 whitespace-pre-wrap break-words text-[11px] leading-4 text-text-muted">
            {summary.detail}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function AgentEventRow({
  event,
}: {
  event: NonNullable<AgentSurfaceItem["events"]>[number];
}) {
  return (
    <div className="grid grid-cols-[12px_1fr_auto] items-start gap-2">
      <span
        className={cn(
          "mt-1.5 size-2 rounded-full",
          event.tone === "success"
            ? "bg-success"
            : event.tone === "danger"
              ? "bg-danger"
              : event.tone === "running"
                ? "animate-pulse bg-accent"
                : "bg-border-strong",
        )}
      />
      <div className="min-w-0">
        <div className="truncate text-xs font-medium text-text">
          {event.label}
        </div>
        {event.detail ? (
          <div className="mt-0.5 line-clamp-2 text-[11px] leading-4 text-text-muted">
            {event.detail}
          </div>
        ) : null}
      </div>
      <time className="pt-0.5 text-[10px] text-text-muted">
        {formatEventTime(event.timestamp)}
      </time>
    </div>
  );
}

function MiniLiveDots({ className }: { className?: string }) {
  return (
    <span
      className={cn("inline-flex items-center gap-0.5", className)}
      aria-hidden="true"
    >
      <span className="size-1 animate-bounce rounded-full bg-current" />
      <span
        className="size-1 animate-bounce rounded-full bg-current"
        style={{ animationDelay: "120ms" }}
      />
      <span
        className="size-1 animate-bounce rounded-full bg-current"
        style={{ animationDelay: "240ms" }}
      />
    </span>
  );
}

function BindingStrip({ state }: { state: WorkSurfaceState }) {
  const workspace = state.workspace;
  const executor = state.executor;
  if (!workspace && !executor) {
    return null;
  }
  return (
    <div className="grid shrink-0 gap-2 border-b border-border/70 px-4 py-3 text-xs md:grid-cols-2">
      <BindingItem
        label="Workspace"
        value={workspaceDisplayName(workspace)}
        detail={workspace?.cwd ?? workspace?.authority}
      />
      <BindingItem
        label="Executor"
        value={executorDisplayName(executor)}
        detail={executorDetail(executor)}
      />
    </div>
  );
}

function RunBlockedBanner({
  blocked,
}: {
  blocked: WorkSurfaceState["blocked"];
}) {
  if (!blocked) {
    return null;
  }
  const executor = executorDisplayName(blocked.executor);
  const workspace =
    blocked.workspace?.cwd ?? workspaceDisplayName(blocked.workspace);
  const fallback = blocked.fallbackPolicy
    ? `Fallback ${statusLabel(blocked.fallbackPolicy)}`
    : undefined;
  const details = [
    executor,
    workspace,
    blocked.transport
      ? `transport ${statusLabel(blocked.transport)}`
      : undefined,
    fallback,
  ].filter((item): item is string => Boolean(item));
  return (
    <div className="border-b border-warning/30 bg-warning/10 px-4 py-3">
      <div className="flex items-start gap-2">
        <AlertTriangle className="mt-0.5 size-4 shrink-0 text-warning" />
        <div className="min-w-0 flex-1">
          <div className="text-xs font-semibold text-text">Run blocked</div>
          <p className="mt-0.5 line-clamp-3 text-xs leading-5 text-text-secondary">
            {blocked.message}
          </p>
          {details.length ? (
            <div className="mt-1 flex flex-wrap gap-x-2 gap-y-1 text-[11px] text-text-muted">
              {details.map((item) => (
                <span key={item} className="max-w-full truncate">
                  {item}
                </span>
              ))}
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

function BindingItem({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail?: string | null;
}) {
  return (
    <div className="min-w-0">
      <div className="text-[10px] font-semibold uppercase text-text-muted">
        {label}
      </div>
      <div className="mt-0.5 truncate font-medium text-text">{value}</div>
      {detail ? (
        <div className="mt-0.5 truncate font-mono text-[11px] text-text-muted">
          {detail}
        </div>
      ) : null}
    </div>
  );
}

function ToolTimeline({
  tools,
  loading,
}: {
  tools: ToolSurfaceItem[];
  loading: boolean;
}) {
  if (!tools.length) {
    return <EmptySurface loading={loading} label="No tool calls yet" />;
  }
  const sorted = [...tools].sort(toolActivitySort);
  return (
    <div className="relative space-y-2.5">
      <div className="absolute bottom-3 left-[17px] top-3 w-px bg-border/80" />
      {sorted.map((tool) => (
        <ToolCard key={tool.callId} tool={tool} />
      ))}
    </div>
  );
}

function ToolCard({ tool }: { tool: ToolSurfaceItem }) {
  const running = tool.status === "running";
  const failed = tool.status === "error";
  const cancelled = tool.status === "cancelled";
  const finishedAt = tool.finishedAt ?? tool.startedAt;
  return (
    <section className="relative pl-9">
      <div
        className={cn(
          "absolute left-[9px] top-3 z-10 flex size-4 items-center justify-center rounded-full border bg-bg",
          running
            ? "border-accent text-accent"
            : cancelled
              ? "border-text-muted text-text-muted"
              : failed
                ? "border-danger text-danger"
                : "border-success text-success",
        )}
      >
        {running ? (
          <span className="size-2 animate-pulse rounded-full bg-accent" />
        ) : cancelled ? (
          <Pause className="size-3" />
        ) : failed ? (
          <AlertTriangle className="size-3" />
        ) : (
          <CheckCircle2 className="size-3" />
        )}
      </div>
      <div
        className={cn(
          "rounded-card border bg-bg p-3 shadow-sm",
          running
            ? "border-accent/35 bg-accent/5"
            : cancelled
              ? "border-border/70 bg-surface-muted/35"
              : failed
                ? "border-danger/25 bg-danger/5"
                : "border-border/70",
        )}
      >
        <div className="flex items-start gap-2">
          <Terminal className="mt-0.5 size-4 shrink-0 text-text-muted" />
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <h3 className="min-w-0 flex-1 truncate font-mono text-xs font-semibold text-text">
                {tool.tool}
              </h3>
              <time className="shrink-0 text-[10px] text-text-muted">
                {formatEventTime(finishedAt)}
              </time>
              <StatusPill status={tool.status} active={running} />
            </div>
            <ToolExecutionMeta tool={tool} />
            {failed ? <ToolFailureNotice tool={tool} /> : null}
            {tool.arguments ? (
              <StructuredPayload label="Args" value={tool.arguments} />
            ) : null}
            {tool.result ? (
              <StructuredPayload
                label="Result"
                value={tool.result}
                tone={failed ? "danger" : "muted"}
              />
            ) : running ? (
              <p className="mt-2 text-xs text-text-muted">Executing...</p>
            ) : null}
          </div>
        </div>
      </div>
    </section>
  );
}

function toolActivitySort(left: ToolSurfaceItem, right: ToolSurfaceItem) {
  const activityDelta = toolActivityAt(right) - toolActivityAt(left);
  if (activityDelta !== 0) {
    return activityDelta;
  }
  const startedDelta = right.startedAt - left.startedAt;
  if (startedDelta !== 0) {
    return startedDelta;
  }
  return right.callId.localeCompare(left.callId);
}

function toolActivityAt(tool: ToolSurfaceItem) {
  return tool.finishedAt ?? tool.startedAt;
}

function ToolExecutionMeta({ tool }: { tool: ToolSurfaceItem }) {
  const items: Array<{
    label: string;
    value: string;
    tone?: "default" | "danger";
  }> = [];
  if (tool.executor) {
    items.push({
      label: "Executor",
      value: executorDisplayName(tool.executor),
      tone: tool.executor.status === "offline" ? "danger" : "default",
    });
  }
  const workspace = tool.workspace?.cwd ?? workspaceDisplayName(tool.workspace);
  if (tool.workspace) {
    items.push({ label: "Workspace", value: workspace });
  }
  if (tool.transport) {
    items.push({ label: "Transport", value: statusLabel(tool.transport) });
  }
  if (tool.fallbackPolicy) {
    items.push({ label: "Fallback", value: statusLabel(tool.fallbackPolicy) });
  }
  if (tool.route) {
    items.push({ label: "Route", value: statusLabel(tool.route) });
  }
  if (tool.durationMs !== undefined) {
    items.push({ label: "Duration", value: formatDuration(tool.durationMs) });
  }
  if (tool.errorKind) {
    items.push({
      label: "Reason",
      value: statusLabel(tool.errorKind),
      tone: tool.errorKind === "cancelled" ? "default" : "danger",
    });
  }
  if (!items.length) {
    return null;
  }
  return (
    <div className="mt-2 grid gap-1.5 sm:grid-cols-2">
      {items.map((item) => (
        <ToolMetaChip
          key={`${item.label}:${item.value}`}
          label={item.label}
          value={item.value}
          tone={item.tone}
        />
      ))}
    </div>
  );
}

function ToolMetaChip({
  label,
  value,
  tone = "default",
}: {
  label: string;
  value: string;
  tone?: "default" | "danger";
}) {
  return (
    <div
      className={cn(
        "min-w-0 rounded-[6px] border px-2 py-1.5",
        tone === "danger"
          ? "border-danger/20 bg-danger/5"
          : "border-border/60 bg-surface-muted",
      )}
      title={`${label}: ${value}`}
    >
      <div
        className={cn(
          "text-[10px] font-semibold uppercase leading-3",
          tone === "danger" ? "text-danger" : "text-text-muted",
        )}
      >
        {label}
      </div>
      <div
        className={cn(
          "mt-0.5 truncate font-mono text-[11px] leading-4",
          tone === "danger" ? "text-danger" : "text-text-secondary",
        )}
      >
        {value}
      </div>
    </div>
  );
}

function ToolFailureNotice({ tool }: { tool: ToolSurfaceItem }) {
  const message = toolFailureMessage(tool);
  if (!message) {
    return null;
  }
  return (
    <div className="mt-2 flex items-start gap-2 rounded-[6px] border border-danger/20 bg-danger/5 px-2 py-2 text-xs leading-5 text-danger">
      <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
      <span className="min-w-0 flex-1">{message}</span>
    </div>
  );
}

function toolFailureMessage(tool: ToolSurfaceItem) {
  if (tool.errorKind === "executor_offline") {
    return "Executor offline. Reconnect the edge executor or choose another workspace.";
  }
  if (tool.errorKind === "transport_disconnected") {
    return "Transport disconnected. Reconnect edge before retrying.";
  }
  if (tool.errorKind === "approval_timeout") {
    return "Approval timed out. Review pending approvals and retry.";
  }
  if (tool.errorKind === "tool_timeout" || tool.errorKind === "timeout") {
    return "Tool timed out. Retry or narrow the command.";
  }
  if (tool.errorKind === "fallback_disabled") {
    return "Fallback disabled. This run will not execute local-code tools on the server.";
  }
  if (tool.errorKind === "workspace_executor_unavailable") {
    return "Workspace executor unavailable. Choose Server sandbox or a connected edge workspace.";
  }
  if (tool.blocked) {
    return "Tool blocked. Resolve the executor or workspace issue before retrying.";
  }
  return undefined;
}

function StructuredPayload({
  label,
  value,
  tone = "muted",
}: {
  label: string;
  value: string;
  tone?: "muted" | "danger";
}) {
  return (
    <div
      className={cn(
        "mt-2 rounded-[6px] border px-2 py-1.5",
        tone === "danger"
          ? "border-danger/20 bg-danger/5"
          : "border-border/60 bg-surface-muted",
      )}
    >
      <div
        className={cn(
          "mb-1 text-[10px] font-semibold uppercase",
          tone === "danger" ? "text-danger" : "text-text-muted",
        )}
      >
        {label}
      </div>
      <StructuredValue value={parseStructuredValue(value)} tone={tone} />
    </div>
  );
}

function StructuredValue({
  value,
  tone,
}: {
  value: unknown;
  tone: "muted" | "danger";
}) {
  if (Array.isArray(value)) {
    return (
      <div className="flex flex-wrap gap-1">
        {value.slice(0, 8).map((item, index) => (
          <span
            key={index}
            className="max-w-full truncate rounded-full bg-bg px-2 py-0.5 font-mono text-[11px] text-text-secondary"
          >
            {formatStructuredScalar(item)}
          </span>
        ))}
        {value.length > 8 ? (
          <span className="text-[11px] text-text-muted">
            +{value.length - 8}
          </span>
        ) : null}
      </div>
    );
  }
  if (isPlainRecord(value)) {
    const entries = Object.entries(value);
    return (
      <div className="space-y-1">
        {entries.slice(0, 8).map(([key, item]) => (
          <div key={key} className="grid min-w-0 grid-cols-[82px_1fr] gap-2">
            <span className="truncate text-[11px] font-medium text-text-muted">
              {key}
            </span>
            <span
              className={cn(
                "min-w-0 truncate font-mono text-[11px]",
                tone === "danger" ? "text-danger" : "text-text-secondary",
              )}
              title={formatStructuredScalar(item)}
            >
              {formatStructuredScalar(item)}
            </span>
          </div>
        ))}
        {entries.length > 8 ? (
          <div className="text-[11px] text-text-muted">
            +{entries.length - 8} fields
          </div>
        ) : null}
      </div>
    );
  }
  return (
    <div
      className={cn(
        "max-h-24 overflow-hidden whitespace-pre-wrap break-words font-mono text-[11px] leading-4",
        tone === "danger" ? "text-danger" : "text-text-secondary",
      )}
    >
      {formatStructuredScalar(value)}
    </div>
  );
}

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-card border border-border/70 bg-bg px-2 py-2">
      <div className="text-base font-semibold text-text">{value}</div>
      <div className="text-[11px] text-text-muted">{label}</div>
    </div>
  );
}

function EmptySurface({ loading, label }: { loading: boolean; label: string }) {
  return (
    <div className="flex min-h-48 flex-col items-center justify-center gap-2 text-center text-sm text-text-muted">
      {loading ? (
        <Loader2 className="size-5 animate-spin" />
      ) : (
        <Pause className="size-5" />
      )}
      <p>{loading ? "Loading work surface" : label}</p>
    </div>
  );
}

function StatusPill({
  status,
  active = false,
}: {
  status: string;
  active?: boolean;
}) {
  const done = isDone(status) || status === "done";
  const cancelled = status === "cancelled";
  const failed = status === "failed" || status === "interrupted";
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium",
        cancelled
          ? "bg-surface-muted text-text-muted"
          : failed
            ? "bg-danger/10 text-danger"
            : done
              ? "bg-success/10 text-success"
              : active
                ? "bg-accent/10 text-accent"
                : "bg-surface-muted text-text-muted",
      )}
    >
      {active ? (
        <span className="size-1.5 animate-pulse rounded-full bg-current" />
      ) : null}
      {statusLabel(status)}
    </span>
  );
}

function StatusIcon({
  status,
  small = false,
}: {
  status: string;
  small?: boolean;
}) {
  const done = isDone(status);
  const running =
    status === "in_progress" || status === "running" || status === "started";
  const cancelled = status === "cancelled";
  const failed = status === "failed" || status === "interrupted";
  const className = cn(
    "shrink-0",
    small ? "size-3" : "mt-0.5 size-4",
    done
      ? "text-success"
      : running
        ? "text-accent"
        : cancelled
          ? "text-text-muted"
          : failed
            ? "text-danger"
            : "text-text-muted",
  );
  if (done) return <CheckCircle2 className={className} />;
  if (cancelled) return <Pause className={className} />;
  if (failed) return <AlertTriangle className={className} />;
  if (running) return <Loader2 className={cn(className, "animate-spin")} />;
  return <Circle className={className} />;
}

function taskCounts(tasks: SessionTask[]) {
  let working = 0;
  let queued = 0;
  let done = 0;
  for (const task of tasks) {
    if (task.status === "in_progress") working += 1;
    else if (task.status === "pending" || task.status === "paused") queued += 1;
    else if (isDone(task.status)) done += 1;
  }
  return { working, queued, done, open: working + queued };
}

function tabItemCount(tab: WorkSurfaceTab, state: WorkSurfaceState) {
  if (tab === "tasks") return state.tasks.length;
  if (tab === "agents") return state.agents.length;
  return state.tools.length;
}

function taskNeedsAttention(task: SessionTask) {
  return Boolean(
    task.status === "blocked" ||
    task.status === "paused" ||
    task.status === "failed" ||
    task.blocked_by?.length,
  );
}

function agentNeedsAttention(agent: AgentSurfaceItem) {
  return (
    agent.status === "failed" ||
    agent.status === "waiting" ||
    agent.status === "interrupted" ||
    Boolean(agent.error)
  );
}

function toolNeedsAttention(tool: ToolSurfaceItem) {
  return Boolean(
    tool.blocked ||
    (tool.status === "error" && tool.errorKind !== "cancelled") ||
    tool.errorKind === "approval_timeout" ||
    tool.errorKind === "tool_timeout" ||
    tool.errorKind === "transport_disconnected" ||
    tool.errorKind === "executor_offline" ||
    tool.errorKind === "fallback_disabled" ||
    tool.errorKind === "workspace_executor_unavailable",
  );
}

function taskSort(left: SessionTask, right: SessionTask) {
  const rank = (task: SessionTask) =>
    task.status === "in_progress"
      ? 0
      : task.status === "pending"
        ? 1
        : task.status === "paused"
          ? 2
          : isDone(task.status)
            ? 4
            : 3;
  return (
    rank(left) - rank(right) ||
    taskUpdatedAtMs(right) - taskUpdatedAtMs(left) ||
    left.id.localeCompare(right.id)
  );
}

function taskUpdatedAtMs(task: SessionTask) {
  const timestamp = Date.parse(task.updated_at);
  return Number.isFinite(timestamp) ? timestamp : 0;
}

function isDone(status: string) {
  return status === "completed" || status === "done" || status === "archived";
}

function isAgentActive(status: string) {
  return ACTIVE_AGENT_SURFACE_STATUSES.has(status);
}

function statusLabel(status: string) {
  return status.replace(/[_-]+/g, " ");
}

function runStatusHeadline(status: string) {
  const label = statusLabel(status).trim();
  return label ? label.charAt(0).toUpperCase() + label.slice(1) : "Active";
}

function workspaceDisplayName(workspace: WorkspaceBindingLike) {
  return (
    workspace?.display_name ??
    (workspace?.kind ? statusLabel(workspace.kind) : "Workspace pending")
  );
}

function executorDisplayName(executor: ExecutorBindingLike) {
  return (
    executor?.display_name ??
    (executor?.kind ? statusLabel(executor.kind) : "Executor pending")
  );
}

function executorDetail(executor: ExecutorBindingLike) {
  const parts = [executor?.transport, executor?.status]
    .filter(Boolean)
    .map((value) => statusLabel(String(value)));
  return parts.length ? parts.join(" / ") : undefined;
}

function formatDuration(ms: number) {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)}s`;
}

function formatEventTime(timestamp: number) {
  const date = new Date(timestamp);
  if (Number.isNaN(date.getTime())) return "now";
  return date.toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function eventType(event: Record<string, unknown>) {
  return stringValue(event.type) ?? stringValue(event.event_type) ?? "event";
}

function eventPayload(event: Record<string, unknown>) {
  return isPlainRecord(event.data) ? event.data : event;
}

function describeRunProjectionEvent(
  event: Record<string, unknown>,
  type: string,
) {
  const payload = eventPayload(event);
  if (type === "text_delta") {
    return {
      label: "Text",
      detail: stringValue(event.content) ?? stringValue(payload.content),
    };
  }
  if (type === "text_done" || type === "turn_complete") {
    return {
      label: "Final text",
      detail:
        stringValue(payload.full_text) ??
        stringValue(event.full_text) ??
        stringValue(payload.assistant_text),
    };
  }
  if (type === "reasoning_delta" || type === "thinking_delta") {
    return {
      label: "Reasoning",
      detail: stringValue(event.content) ?? stringValue(payload.content),
    };
  }
  if (type === "tool_call") {
    const toolCall = isPlainRecord(event.tool_call) ? event.tool_call : {};
    const fn = isPlainRecord(toolCall.function) ? toolCall.function : {};
    return {
      label: `Tool ${stringValue(fn.name) ?? stringValue(toolCall.name) ?? "call"}`,
      detail: formatStructuredScalar(fn.arguments ?? toolCall.arguments),
    };
  }
  if (type === "tool_call_start") {
    return {
      label: `Tool ${stringValue(event.tool) ?? "started"}`,
      detail: formatStructuredScalar(event.arguments),
    };
  }
  if (type === "tool_call_end") {
    return {
      label: event.success === false ? "Tool failed" : "Tool completed",
      detail: formatStructuredScalar(event.result),
    };
  }
  if (type === "run_error") {
    return {
      label: "Run error",
      detail: stringValue(payload.error),
    };
  }
  if (type === "run_finished") {
    return {
      label: "Run finished",
      detail: payload.cancelled
        ? "cancelled"
        : payload.interrupted
          ? "interrupted"
          : undefined,
    };
  }
  return {
    label: statusLabel(type),
    detail: formatStructuredScalar(payload),
  };
}

function stringValue(value: unknown) {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function parseStructuredValue(value: string): unknown {
  const trimmed = value.trim();
  if (!trimmed) return value;
  if (
    (trimmed.startsWith("{") && trimmed.endsWith("}")) ||
    (trimmed.startsWith("[") && trimmed.endsWith("]"))
  ) {
    try {
      return JSON.parse(trimmed);
    } catch {
      return value;
    }
  }
  return value;
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function formatStructuredScalar(value: unknown): string {
  if (value === null) return "null";
  if (value === undefined) return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}
