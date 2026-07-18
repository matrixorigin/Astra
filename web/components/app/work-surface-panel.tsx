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
  Sparkles,
  Terminal,
  Wrench,
  X,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  ACTIVE_AGENT_SURFACE_STATUSES,
  agentInterruptionPresentation,
} from "@/lib/work-surface";
import type {
  AgentSurfaceItem,
  ExecutorBinding,
  SessionTask,
  ToolSurfaceItem,
  WorkspaceBinding,
  WorkSurfaceState,
} from "@/lib/work-surface";
import type {
  ChatInsightsResponse,
  WorkSurfaceRunResponse,
} from "@/lib/api/types";
import { WORKSPACE_EXECUTION_BLOCKED_MESSAGE } from "@/lib/run-status-messages";
import { cn } from "@/lib/utils/cn";

export type WorkSurfaceTab = "tasks" | "agents" | "tools" | "insights";
type WorkSurfaceViewMode = "all" | "attention";

type WorkSurfacePanelProps = {
  state: WorkSurfaceState;
  activeRun?: { runId: string; status: string; waitingFor?: string | null };
  tab: WorkSurfaceTab;
  onTabChange: (tab: WorkSurfaceTab) => void;
  defaultCollapsed?: boolean;
  openSignal?: number;
  onRefresh: () => void;
  onLoadAgentRun: (runId: string) => Promise<WorkSurfaceRunResponse>;
  onLoadInsights?: () => Promise<ChatInsightsResponse>;
};

type AgentRunProjectionState = {
  loading: boolean;
  error: string | null;
  projection: WorkSurfaceRunResponse | null;
};

type WorkspaceBindingLike = Partial<WorkspaceBinding> | null | undefined;
type ExecutorBindingLike = Partial<ExecutorBinding> | null | undefined;

export function WorkSurfacePanel({
  state,
  activeRun,
  tab,
  onTabChange,
  defaultCollapsed = false,
  openSignal,
  onRefresh,
  onLoadAgentRun,
  onLoadInsights = async () => {
    throw new Error("Session insights are not available on this surface.");
  },
}: WorkSurfacePanelProps) {
  const [collapsed, setCollapsed] = useState(defaultCollapsed);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [transcriptAgentId, setTranscriptAgentId] = useState<string | null>(
    null,
  );
  const [viewModes, setViewModes] = useState<
    Record<WorkSurfaceTab, WorkSurfaceViewMode>
  >({
    tasks: "all",
    agents: "all",
    tools: "all",
    insights: "all",
  });
  const [agentRunDetails, setAgentRunDetails] = useState<
    Record<string, AgentRunProjectionState>
  >({});
  const [insightsState, setInsightsState] = useState<{
    loading: boolean;
    error: string | null;
    payload: ChatInsightsResponse | null;
  }>({ loading: false, error: null, payload: null });
  const openSignalRef = useRef(openSignal);
  const counts = useMemo(() => taskCounts(state.tasks), [state.tasks]);
  const attentionCounts = useMemo(
    () => ({
      tasks: state.tasks.filter(taskNeedsAttention).length,
      agents: state.agents.filter(agentNeedsAttention).length,
      tools: state.tools.filter(toolNeedsAttention).length,
      insights: 0,
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
        : tab === "tools"
          ? toolViewMode
          : "all";
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
  const insightCount = insightRecommendationCount(insightsState.payload);
  const visibleRunStatus = activeRun?.status ?? state.runStatus;
  const workbenchActivity = workbenchActivitySummary(state);
  const selectedAgent = selectedAgentId
    ? visibleAgents.find((agent) => agent.agentId === selectedAgentId)
    : undefined;
  const transcriptAgent = transcriptAgentId
    ? state.agents.find((agent) => agent.agentId === transcriptAgentId)
    : undefined;
  const projectedAgent = transcriptAgent ?? selectedAgent;

  const toggleWorkSurface = useCallback(() => {
    if (window.innerWidth < 1024) {
      setMobileOpen((value) => !value);
      return;
    }
    setCollapsed((value) => !value);
  }, []);

  const openWorkSurfaceTab = useCallback(
    (nextTab: WorkSurfaceTab) => {
      onTabChange(nextTab);
      if (window.innerWidth < 1024) {
        setMobileOpen(true);
        return;
      }
      setCollapsed(false);
    },
    [onTabChange],
  );

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
    const firstActive =
      visibleAgents.find((agent) => isAgentActive(agent.status)) ??
      visibleAgents[0];
    setSelectedAgentId(firstActive?.agentId ?? null);
  }, [selectedAgentId, visibleAgents, tab]);

  useEffect(() => {
    if (!projectedAgent?.runId) {
      return;
    }
    let cancelled = false;
    let timeout: number | undefined;
    let intervalMs = 2_500;
    const runId = projectedAgent.runId;
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
    if (isAgentActive(projectedAgent.status)) {
      timeout = window.setTimeout(scheduleNext, intervalMs);
    }

    return () => {
      cancelled = true;
      if (timeout) {
        window.clearTimeout(timeout);
      }
    };
  }, [onLoadAgentRun, projectedAgent?.runId, projectedAgent?.status]);

  const loadInsights = useCallback(async () => {
    setInsightsState((current) => ({
      ...current,
      loading: true,
      error: null,
    }));
    try {
      const payload = await onLoadInsights();
      setInsightsState({ loading: false, error: null, payload });
    } catch (error) {
      setInsightsState((current) => ({
        ...current,
        loading: false,
        error:
          error instanceof Error ? error.message : "Failed to load insights.",
      }));
    }
  }, [onLoadInsights]);

  useEffect(() => {
    if (tab !== "insights" || insightsState.loading || insightsState.payload) {
      return;
    }
    void loadInsights();
  }, [insightsState.loading, insightsState.payload, loadInsights, tab]);

  useEffect(() => {
    if (openSignal === undefined || openSignalRef.current === openSignal) {
      return;
    }
    openSignalRef.current = openSignal;
    if (window.innerWidth < 1024) {
      setMobileOpen(true);
    } else {
      setCollapsed(false);
    }
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
      <div className="flex h-16 shrink-0 items-center gap-2 border-b border-border/70 px-4">
        <button
          type="button"
          className="hidden size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text lg:inline-flex"
          onClick={toggleWorkSurface}
          aria-label={
            collapsed ? "Expand activity panel" : "Collapse activity panel"
          }
        >
          <ChevronRight
            className={cn("size-4 transition", collapsed ? "rotate-180" : "")}
          />
        </button>
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span
              className={cn(
                "relative flex size-5 shrink-0 items-center justify-center rounded-[6px] bg-accent/10 text-accent",
                visibleRunStatus && !isTerminalRunStatus(visibleRunStatus)
                  ? "after:absolute after:-right-0.5 after:-top-0.5 after:size-1.5 after:animate-pulse after:rounded-full after:bg-accent after:ring-2 after:ring-surface"
                  : "",
              )}
            >
              <Activity className="size-3.5" />
            </span>
            <h2 className="truncate text-sm font-semibold tracking-[-0.01em] text-text">
              Astra Workbench
            </h2>
          </div>
          <div className="mt-0.5 flex min-w-0 items-center gap-1.5 truncate text-xs text-text-muted">
            <span className="shrink-0 text-text-secondary">
              {visibleRunStatus
                ? runStatusHeadline(visibleRunStatus)
                : state.hydrated
                  ? "Session activity"
                  : "Waiting for session"}
            </span>
            {workbenchActivity ? (
              <>
                <span aria-hidden="true">·</span>
                <span className="truncate">{workbenchActivity}</span>
              </>
            ) : null}
          </div>
        </div>
        <button
          type="button"
          className="inline-flex size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text"
          onClick={onRefresh}
          aria-label="Refresh activity"
        >
          <RotateCw className={cn("size-4", state.loading && "animate-spin")} />
        </button>
        <button
          type="button"
          className="inline-flex size-8 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text lg:hidden"
          onClick={() => setMobileOpen(false)}
          aria-label="Close activity"
        >
          <X className="size-4" />
        </button>
      </div>

      <RunBlockedBanner blocked={state.blocked} />

      <div className="flex shrink-0 gap-1 border-b border-border/70 bg-bg/45 px-2 py-2">
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
        <TabButton
          active={tab === "insights"}
          icon={Sparkles}
          label="Insights"
          count={insightCount}
          onClick={() => onTabChange("insights")}
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
            allTasks={state.tasks}
            loading={state.loading}
            counts={visibleTaskCounts}
          />
        ) : null}
        {tab === "agents" ? (
          <AgentBoard
            agents={visibleAgents}
            loading={state.loading}
            selectedAgentId={selectedAgentId}
            rootRunId={state.runId}
            agentRunDetails={agentRunDetails}
            onSelectAgent={(agentId) =>
              setSelectedAgentId((current) =>
                current === agentId ? null : agentId,
              )
            }
            onOpenTranscript={(agentId) => {
              setSelectedAgentId(agentId);
              setTranscriptAgentId(agentId);
            }}
          />
        ) : null}
        {tab === "tools" ? (
          <ToolTimeline tools={visibleTools} loading={state.loading} />
        ) : null}
        {tab === "insights" ? (
          <InsightsBoard
            state={insightsState}
            onRefresh={() => {
              void loadInsights();
            }}
          />
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
        Activity
      </button>
      <aside
        className={cn(
          "hidden h-full shrink-0 border-l border-border/70 bg-surface/70 backdrop-blur lg:flex lg:flex-col",
          collapsed ? "w-[58px]" : "w-[360px] xl:w-[400px]",
        )}
      >
        {collapsed ? (
          <div className="flex h-full w-full flex-col items-center gap-1 py-3">
            <button
              type="button"
              className="mb-2 inline-flex size-9 items-center justify-center rounded-control text-text-muted transition hover:bg-surface-muted hover:text-text"
              onClick={toggleWorkSurface}
              aria-label="Expand run workspace"
              title="Run workspace"
            >
              <Activity className="size-4" />
            </button>
            <CollapsedTabButton
              active={tab === "tasks"}
              icon={ClipboardList}
              label="Tasks"
              count={counts.open}
              onClick={() => openWorkSurfaceTab("tasks")}
            />
            <CollapsedTabButton
              active={tab === "agents"}
              icon={Bot}
              label="Agents"
              count={agentActivityCount}
              onClick={() => openWorkSurfaceTab("agents")}
            />
            <CollapsedTabButton
              active={tab === "tools"}
              icon={Terminal}
              label="Tools"
              count={toolActivityCount}
              onClick={() => openWorkSurfaceTab("tools")}
            />
            <CollapsedTabButton
              active={tab === "insights"}
              icon={Sparkles}
              label="Insights"
              count={insightCount}
              onClick={() => openWorkSurfaceTab("insights")}
            />
          </div>
        ) : (
          body
        )}
      </aside>
      {mobileOpen ? (
        <div className="fixed inset-0 z-40 lg:hidden">
          <button
            type="button"
            className="absolute inset-0 bg-black/20"
            aria-label="Close activity"
            onClick={() => setMobileOpen(false)}
          />
          <aside className="absolute inset-y-0 right-0 flex w-[min(100vw,390px)] flex-col border-l border-border bg-surface shadow-2xl">
            {body}
          </aside>
        </div>
      ) : null}
      {transcriptAgent ? (
        <AgentTranscriptWorkspace
          agent={transcriptAgent}
          agents={state.agents}
          rootRunId={state.runId}
          details={
            transcriptAgent.runId
              ? agentRunDetails[transcriptAgent.runId]
              : undefined
          }
          onSelectAgent={(agentId) => {
            setSelectedAgentId(agentId);
            setTranscriptAgentId(agentId);
          }}
          onClose={() => setTranscriptAgentId(null)}
        />
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
        "relative inline-flex flex-1 items-center justify-center gap-1.5 rounded-control px-2.5 py-2 text-xs font-medium transition",
        active
          ? "bg-surface text-text shadow-[0_1px_3px_rgba(15,23,42,0.08)] ring-1 ring-border/70 after:absolute after:inset-x-3 after:-bottom-2 after:h-0.5 after:rounded-full after:bg-accent"
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

function CollapsedTabButton({
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
      aria-label={`Open ${label.toLowerCase()} workspace`}
      title={label}
      className={cn(
        "relative inline-flex size-10 items-center justify-center rounded-control transition",
        active
          ? "bg-accent/10 text-accent"
          : "text-text-muted hover:bg-surface-muted hover:text-text",
      )}
    >
      <Icon className="size-4" />
      {count > 0 ? (
        <span className="absolute right-0.5 top-0.5 min-w-3.5 rounded-full bg-text px-1 text-center text-[9px] font-semibold leading-3.5 text-white">
          {count > 9 ? "9+" : count}
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

function InsightsBoard({
  state,
  onRefresh,
}: {
  state: {
    loading: boolean;
    error: string | null;
    payload: ChatInsightsResponse | null;
  };
  onRefresh: () => void;
}) {
  const payload = state.payload;
  if (state.loading && !payload) {
    return <EmptySurface loading label="Reflecting on durable evidence" />;
  }
  if (state.error && !payload) {
    return (
      <div className="rounded-card border border-danger/20 bg-danger/5 p-4">
        <p className="text-sm font-medium text-danger">
          Insights are unavailable
        </p>
        <p className="mt-1 text-xs leading-5 text-text-muted">{state.error}</p>
        <button
          type="button"
          onClick={onRefresh}
          className="mt-3 inline-flex items-center gap-1.5 rounded-control border border-border bg-surface px-3 py-1.5 text-xs font-medium text-text hover:bg-surface-muted"
        >
          <RotateCw className="size-3.5" />
          Retry
        </button>
      </div>
    );
  }
  if (!payload) {
    return <EmptySurface loading={false} label="No durable evidence yet" />;
  }

  const audit = payload.audit;
  const recommendations = Array.isArray(payload.reflection?.recommendations)
    ? payload.reflection.recommendations
    : [];
  const reflectionFacts = insightOverviewRows(payload.reflection?.overview);
  const decisionFacts = insightOverviewRows(payload.decisionTrace?.overview);

  return (
    <div className="space-y-4">
      <section className="rounded-card border border-border/70 bg-bg p-3 shadow-sm">
        <div className="flex items-start gap-3">
          <div className="flex size-8 shrink-0 items-center justify-center rounded-full border border-accent/20 bg-accent/10 text-accent">
            <Sparkles className="size-4" />
          </div>
          <div className="min-w-0 flex-1">
            <h3 className="text-sm font-semibold text-text">
              Evidence-backed session view
            </h3>
            <p className="mt-1 text-xs leading-5 text-text-muted">
              Durable audit, reflection, and routing evidence. Refreshing this
              view does not alter the run.
            </p>
          </div>
          <button
            type="button"
            onClick={onRefresh}
            disabled={state.loading}
            className="inline-flex size-8 shrink-0 items-center justify-center rounded-control text-text-muted hover:bg-surface-muted hover:text-text disabled:opacity-50"
            aria-label="Refresh session insights"
          >
            <RotateCw
              className={cn("size-4", state.loading && "animate-spin")}
            />
          </button>
        </div>
      </section>

      {audit ? (
        <section>
          <div className="mb-2 flex items-center justify-between">
            <h3 className="text-xs font-semibold uppercase tracking-[0.08em] text-text-muted">
              Audit
            </h3>
            <span className="text-[11px] text-text-muted">{audit.status}</span>
          </div>
          <div className="grid grid-cols-2 gap-2">
            <Metric label="Turns" value={audit.turn_count} />
            <Metric label="Tool calls" value={audit.tool_calls_total} />
            <Metric label="Tool failures" value={audit.tool_calls_failed} />
            <Metric label="Compactions" value={audit.compact_count} />
          </div>
        </section>
      ) : null}

      <InsightSection
        title="Reflect"
        emptyLabel="No recommendations were produced for the current evidence."
        rows={
          recommendations.length
            ? recommendations.map((recommendation, index) => ({
                key: `recommendation-${index}`,
                label: recommendation,
              }))
            : reflectionFacts
        }
      />

      <InsightSection
        title="Decision trace"
        emptyLabel="No material routing decisions were reported."
        rows={decisionFacts}
      />

      {payload.warnings.length ? (
        <section className="rounded-card border border-warning/20 bg-warning/5 p-3">
          <h3 className="text-xs font-semibold text-warning">
            Partial evidence
          </h3>
          <ul className="mt-2 space-y-1 text-xs leading-5 text-text-muted">
            {payload.warnings.map((warning) => (
              <li key={warning}>{warning}</li>
            ))}
          </ul>
        </section>
      ) : null}

      <p className="text-center text-[11px] text-text-muted">
        Refreshed {formatInsightTime(payload.generatedAt)}
      </p>
    </div>
  );
}

type InsightRow = { key: string; label: string; value?: string };

function InsightSection({
  title,
  rows,
  emptyLabel,
}: {
  title: string;
  rows: InsightRow[];
  emptyLabel: string;
}) {
  return (
    <section className="rounded-card border border-border/70 bg-bg p-3 shadow-sm">
      <h3 className="text-xs font-semibold uppercase tracking-[0.08em] text-text-muted">
        {title}
      </h3>
      {rows.length ? (
        <div className="mt-2 divide-y divide-border/60">
          {rows.slice(0, 8).map((row) => (
            <div key={row.key} className="py-2 first:pt-1 last:pb-1">
              <p className="text-xs leading-5 text-text-secondary">
                {row.label}
              </p>
              {row.value ? (
                <p className="mt-0.5 break-words font-mono text-[11px] leading-4 text-text-muted">
                  {row.value}
                </p>
              ) : null}
            </div>
          ))}
        </div>
      ) : (
        <p className="mt-2 text-xs leading-5 text-text-muted">{emptyLabel}</p>
      )}
    </section>
  );
}

function insightOverviewRows(
  overview: Record<string, unknown> | null | undefined,
): InsightRow[] {
  if (!overview) {
    return [];
  }
  return Object.entries(overview)
    .map(([key, value]) => ({
      key,
      label: statusLabel(key),
      value: compactInsightValue(value),
    }))
    .filter((row) => Boolean(row.value));
}

function compactInsightValue(value: unknown): string | undefined {
  if (typeof value === "string") {
    return value.trim() || undefined;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    const values = value
      .map((item) => compactInsightValue(item))
      .filter((item): item is string => Boolean(item));
    return values.length ? values.slice(0, 6).join(" · ") : undefined;
  }
  if (isPlainRecord(value)) {
    const entries = Object.entries(value)
      .map(([key, item]) => {
        const formatted = compactInsightValue(item);
        return formatted ? `${statusLabel(key)}: ${formatted}` : null;
      })
      .filter((item): item is string => Boolean(item));
    return entries.length ? entries.slice(0, 6).join(" · ") : undefined;
  }
  return undefined;
}

function formatInsightTime(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? "just now"
    : date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function TaskBoard({
  tasks,
  allTasks,
  loading,
  counts,
}: {
  tasks: SessionTask[];
  allTasks: SessionTask[];
  loading: boolean;
  counts: ReturnType<typeof taskCounts>;
}) {
  if (!tasks.length) {
    return <EmptySurface loading={loading} label="No tasks yet" />;
  }
  const sorted = [...tasks].sort(taskSort);
  const tasksById = new Map(allTasks.map((task) => [task.id, task]));
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-3 divide-x divide-border/70 rounded-card border border-border/70 bg-bg p-1 text-center shadow-sm">
        <Metric label="Working" value={counts.working} />
        <Metric label="Queued" value={counts.queued} />
        <Metric label="Done" value={counts.done} />
      </div>
      <div className="space-y-2.5">
        {sorted.map((task) => (
          <TaskCard key={task.id} task={task} tasksById={tasksById} />
        ))}
      </div>
    </div>
  );
}

function TaskCard({
  task,
  tasksById,
}: {
  task: SessionTask;
  tasksById: Map<string, SessionTask>;
}) {
  const subtasks = task.subtasks ?? [];
  const done = subtasks.filter((item) => isDone(item.status)).length;
  const progress = taskObservedProgress(task, done);
  const running = task.status === "in_progress";
  return (
    <section
      className={cn(
        "rounded-card border bg-bg p-3 shadow-sm transition-colors",
        taskNeedsAttention(task)
          ? "border-warning/30 bg-warning/[0.025]"
          : running
            ? "border-accent/25 bg-accent/[0.02]"
            : "border-border/70",
      )}
    >
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
          {progress ? (
            <div>
              <div className="mb-1 flex items-center justify-between gap-3 text-[10px] font-medium text-text-muted">
                <span>{progress.label}</span>
                <span className="tabular-nums">{progress.value}%</span>
              </div>
              <div className="h-1 overflow-hidden rounded-full bg-surface-muted">
                <div
                  className={cn(
                    "h-full rounded-full transition-all duration-500",
                    isDone(task.status)
                      ? "bg-success"
                      : running
                        ? "bg-accent"
                        : "bg-border-strong",
                  )}
                  style={{ width: `${progress.value}%` }}
                />
              </div>
            </div>
          ) : null}
          <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-text-muted">
            {subtasks.length ? (
              <span>
                {done}/{subtasks.length} subtasks
              </span>
            ) : (
              <span>Updated {formatTaskUpdatedAt(task.updated_at)}</span>
            )}
            {task.owner ? <span>Owner {task.owner}</span> : null}
            {task.blocks?.length ? (
              <span>Unblocks {task.blocks.length}</span>
            ) : null}
          </div>
          {task.blocked_by?.length ? (
            <TaskDependencyStrip task={task} tasksById={tasksById} />
          ) : null}
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
                    title={subtask.title}
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

function TaskDependencyStrip({
  task,
  tasksById,
}: {
  task: SessionTask;
  tasksById: Map<string, SessionTask>;
}) {
  const blockers = (task.blocked_by ?? []).map((id) => ({
    id,
    title: tasksById.get(id)?.title ?? id,
  }));
  return (
    <div className="rounded-[7px] border border-warning/20 bg-warning/[0.055] px-2.5 py-2">
      <div className="flex items-start gap-2">
        <Pause className="mt-0.5 size-3.5 shrink-0 text-warning" />
        <div className="min-w-0 flex-1">
          <p className="text-[10px] font-semibold uppercase tracking-[0.08em] text-warning">
            Blocked by
          </p>
          <p className="mt-1 line-clamp-2 text-xs leading-5 text-text-secondary">
            {blockers.map((blocker) => blocker.title).join(" · ")}
          </p>
        </div>
      </div>
    </div>
  );
}

function AgentBoard({
  agents,
  loading,
  selectedAgentId,
  rootRunId,
  agentRunDetails,
  onSelectAgent,
  onOpenTranscript,
}: {
  agents: AgentSurfaceItem[];
  loading: boolean;
  selectedAgentId: string | null;
  rootRunId: string | null;
  agentRunDetails: Record<string, AgentRunProjectionState>;
  onSelectAgent: (agentId: string) => void;
  onOpenTranscript: (agentId: string) => void;
}) {
  if (!agents.length) {
    return <EmptySurface loading={loading} label="No subagent activity yet" />;
  }
  const hierarchy = agentHierarchyRows(agents, rootRunId);
  const summary = agentRunSummary(agents);
  return (
    <div className="space-y-3">
      <AgentRunSummary summary={summary} />
      <div className="space-y-2.5">
        {hierarchy.map((row) => (
          <div
            key={row.agent.agentId}
            className={cn(
              row.depth > 0 && "relative border-l border-border/80 pl-2",
            )}
            style={{ marginLeft: `${Math.min(row.depth, 3) * 10}px` }}
          >
            {row.depth > 0 ? (
              <span className="absolute -left-px top-5 h-px w-2 bg-border" />
            ) : null}
            <AgentCard
              agent={row.agent}
              parentLabel={row.parentLabel}
              childCount={row.childCount}
              selected={selectedAgentId === row.agent.agentId}
              runDetails={
                row.agent.runId ? agentRunDetails[row.agent.runId] : undefined
              }
              onSelect={() => onSelectAgent(row.agent.agentId)}
              onOpenTranscript={() => onOpenTranscript(row.agent.agentId)}
            />
          </div>
        ))}
      </div>
    </div>
  );
}

function AgentRunSummary({
  summary,
}: {
  summary: ReturnType<typeof agentRunSummary>;
}) {
  return (
    <div className="flex items-center gap-3 rounded-[8px] border border-border/70 bg-bg px-3 py-2 shadow-sm">
      <span className="relative flex size-7 shrink-0 items-center justify-center rounded-full bg-accent/10 text-accent">
        <Bot className="size-3.5" />
        {summary.active ? (
          <span className="absolute -right-0.5 -top-0.5 size-2 animate-pulse rounded-full bg-accent ring-2 ring-bg" />
        ) : null}
      </span>
      <div className="min-w-0 flex-1">
        <p className="text-xs font-semibold text-text">Run map</p>
        <p className="mt-0.5 truncate text-[11px] text-text-muted">
          {agentRunSummaryText(summary)}
        </p>
      </div>
      <span className="text-xs font-semibold tabular-nums text-text-secondary">
        {summary.total}
      </span>
    </div>
  );
}

type AgentHierarchyRow = {
  agent: AgentSurfaceItem;
  depth: number;
  parentLabel: string;
  childCount: number;
};

function agentHierarchyRows(
  agents: AgentSurfaceItem[],
  rootRunId: string | null,
): AgentHierarchyRow[] {
  const byRunId = new Map(
    agents
      .filter((agent): agent is AgentSurfaceItem & { runId: string } =>
        Boolean(agent.runId),
      )
      .map((agent) => [agent.runId, agent]),
  );
  const childrenByParent = new Map<string, AgentSurfaceItem[]>();
  for (const agent of agents) {
    if (!agent.parentRunId) continue;
    const children = childrenByParent.get(agent.parentRunId) ?? [];
    children.push(agent);
    childrenByParent.set(agent.parentRunId, children);
  }

  const roots = agents.filter(
    (agent) =>
      !agent.parentRunId ||
      agent.parentRunId === rootRunId ||
      !byRunId.has(agent.parentRunId),
  );
  const rows: AgentHierarchyRow[] = [];
  const visited = new Set<string>();
  const visit = (agent: AgentSurfaceItem, depth: number) => {
    if (visited.has(agent.agentId)) return;
    visited.add(agent.agentId);
    const children = agent.runId
      ? (childrenByParent.get(agent.runId) ?? [])
      : [];
    rows.push({
      agent,
      depth,
      parentLabel: agentParentLabel(agent, agents, rootRunId),
      childCount: children.length,
    });
    children.forEach((child) => visit(child, depth + 1));
  };
  roots.forEach((agent) => visit(agent, 0));
  // Corrupt or cyclic ancestry must remain observable instead of disappearing.
  agents.forEach((agent) => visit(agent, 0));
  return rows;
}

function agentParentLabel(
  agent: AgentSurfaceItem,
  agents: AgentSurfaceItem[],
  rootRunId: string | null,
) {
  if (!agent.parentRunId || agent.parentRunId === rootRunId) {
    return "Main conversation";
  }
  const parent = agents.find(
    (candidate) => candidate.runId === agent.parentRunId,
  );
  return (
    parent?.description || parent?.agentType || shortRunId(agent.parentRunId)
  );
}

function agentRunSummary(agents: AgentSurfaceItem[]) {
  return {
    total: agents.length,
    active: agents.filter((agent) => isAgentActive(agent.status)).length,
    completed: agents.filter(
      (agent) => agent.status.trim().toLowerCase() === "completed",
    ).length,
    attention: agents.filter(agentNeedsAttention).length,
  };
}

function agentRunSummaryText(summary: ReturnType<typeof agentRunSummary>) {
  const parts: string[] = [];
  if (summary.active) parts.push(`${summary.active} working`);
  if (summary.attention) parts.push(`${summary.attention} need attention`);
  if (summary.completed) parts.push(`${summary.completed} complete`);
  return parts.length ? parts.join(" · ") : `${summary.total} observed`;
}

function shortRunId(value: string) {
  return value.length > 12 ? `${value.slice(0, 8)}…` : value;
}

function AgentCard({
  agent,
  parentLabel,
  childCount,
  selected,
  runDetails,
  onSelect,
  onOpenTranscript,
}: {
  agent: AgentSurfaceItem;
  parentLabel: string;
  childCount: number;
  selected: boolean;
  runDetails?: AgentRunProjectionState;
  onSelect: () => void;
  onOpenTranscript: () => void;
}) {
  const active = isAgentActive(agent.status);
  const display = agentDisplayState(agent);
  const waiting = display.tone === "warning";
  const failed = display.tone === "danger";
  const turnBudgetUsed =
    agent.turn && agent.maxTurns
      ? Math.min(100, Math.round((agent.turn / agent.maxTurns) * 100))
      : undefined;
  const summary = agent.resultSummary ?? agent.error ?? agent.reason;
  const latestEvent = agent.events?.[agent.events.length - 1];
  const title = agent.description || agent.agentType || "Subagent";
  const metaItems = agentCompactMeta(agent);
  return (
    <section
      className={cn(
        "overflow-hidden rounded-card border bg-bg shadow-sm transition-colors",
        selected ? "ring-1 ring-accent/25" : "",
        active
          ? "border-accent/30"
          : waiting
            ? "border-warning/25"
            : failed
              ? "border-danger/25"
              : "border-border/70",
      )}
    >
      <button
        type="button"
        className="group flex w-full items-start gap-3 p-3 text-left transition-colors hover:bg-surface/70"
        onClick={onSelect}
        aria-expanded={selected}
      >
        <div
          className={cn(
            "mt-0.5 flex size-8 shrink-0 items-center justify-center rounded-full border",
            active
              ? "border-accent/25 bg-accent/10 text-accent"
              : waiting
                ? "border-warning/25 bg-warning/10 text-warning"
                : failed
                  ? "border-danger/20 bg-danger/10 text-danger"
                  : "border-border bg-surface-muted text-text-muted",
          )}
        >
          {active ? (
            <Activity className="size-4 animate-pulse" />
          ) : waiting ? (
            <Pause className="size-4" />
          ) : (
            <Bot className="size-4" />
          )}
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-start gap-2">
            <div className="min-w-0 flex-1 space-y-1">
              <h3 className="line-clamp-2 text-sm font-semibold leading-5 text-text">
                {title}
              </h3>
              <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-text-muted">
                <span className="truncate">Child of {parentLabel}</span>
                {agent.agentType ? (
                  <span>{statusLabel(agent.agentType)}</span>
                ) : null}
                {childCount ? (
                  <span>
                    {childCount} child run{childCount === 1 ? "" : "s"}
                  </span>
                ) : null}
                {metaItems.map((item) => (
                  <span key={item} className="min-w-0 max-w-full truncate">
                    {item}
                  </span>
                ))}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-1.5">
              <StatusPill
                status={agent.status}
                active={active}
                label={display.label}
                tone={active ? "running" : display.tone}
              />
              <ChevronRight
                className={cn(
                  "size-4 text-text-muted transition-transform",
                  selected ? "rotate-90" : "group-hover:translate-x-0.5",
                )}
              />
            </div>
          </div>
          {latestEvent ? (
            <div className="mt-2 flex min-w-0 items-center gap-1.5 rounded-[6px] bg-surface-muted/70 px-2 py-1.5 text-xs text-text-secondary">
              {active ? <MiniLiveDots className="shrink-0" /> : null}
              <span
                className={cn(
                  "shrink-0 text-[11px] font-medium",
                  failed
                    ? "text-danger"
                    : waiting
                      ? "text-warning"
                      : active
                        ? "text-accent"
                        : "text-text-muted",
                )}
              >
                {latestEvent.tone === "danger" ? "Issue" : "Latest"}
              </span>
              <span className="min-w-0 flex-1 truncate">
                {latestEvent.label}
              </span>
            </div>
          ) : active ? (
            <div className="mt-2 flex min-w-0 items-center gap-1.5 rounded-[6px] bg-surface-muted/70 px-2 py-1.5 text-xs text-text-muted">
              <MiniLiveDots className="shrink-0" />
              <span>
                {agent.toolName
                  ? `Running ${agent.toolName}`
                  : "Waiting for subagent progress"}
              </span>
            </div>
          ) : null}
          {turnBudgetUsed !== undefined ? (
            <div className="mt-2">
              <div className="mb-1 flex items-center justify-between gap-3 text-[10px] font-medium text-text-muted">
                <span>Turn budget</span>
                <span className="tabular-nums">
                  {agent.turn}/{agent.maxTurns}
                </span>
              </div>
              <div className="h-1 overflow-hidden rounded-full bg-surface-muted">
                <div
                  className={cn(
                    "h-full rounded-full transition-all duration-500",
                    failed ? "bg-danger" : waiting ? "bg-warning" : "bg-accent",
                  )}
                  style={{ width: `${turnBudgetUsed}%` }}
                />
              </div>
            </div>
          ) : null}
          {summary ? (
            <p
              className={cn(
                "mt-2 line-clamp-2 text-xs leading-5",
                failed
                  ? "text-danger"
                  : waiting
                    ? "text-warning"
                    : "text-text-muted",
              )}
            >
              {summary}
            </p>
          ) : null}
        </div>
      </button>
      {selected ? (
        <AgentDetails
          agent={agent}
          failed={failed}
          runDetails={runDetails}
          display={display}
          onOpenTranscript={onOpenTranscript}
        />
      ) : null}
    </section>
  );
}

type AgentDisplayTone =
  "neutral" | "running" | "success" | "warning" | "danger";

function agentDisplayState(agent: AgentSurfaceItem): {
  label: string;
  tone: AgentDisplayTone;
} {
  const status = agent.status.trim().toLowerCase();
  if (ACTIVE_AGENT_SURFACE_STATUSES.has(status)) {
    return { label: "Working", tone: "running" };
  }
  if (status === "completed") {
    return { label: "Completed", tone: "success" };
  }
  if (status === "waiting") {
    return { label: "Waiting", tone: "warning" };
  }
  if (status === "failed") {
    return { label: "Failed", tone: "danger" };
  }
  if (status === "cancelled") {
    return { label: "Cancelled", tone: "danger" };
  }
  if (status === "interrupted") {
    const display = agentInterruptionPresentation(agent.reason);
    return {
      label: display.label,
      tone: display.tone === "danger" ? "danger" : "warning",
    };
  }
  return { label: statusLabel(agent.status), tone: "neutral" };
}

function agentCompactMeta(agent: AgentSurfaceItem) {
  const items: string[] = [];
  if (agent.turn) {
    items.push(
      `Turn ${agent.turn}${agent.maxTurns ? `/${agent.maxTurns}` : ""}`,
    );
  }
  if (agent.toolName) {
    items.push(agent.toolName);
  }
  if (agent.totalToolCalls) {
    items.push(`${agent.totalToolCalls} tools`);
  }
  if (agent.totalPromptTokens || agent.totalCompletionTokens) {
    items.push(
      `${(agent.totalPromptTokens ?? 0) + (agent.totalCompletionTokens ?? 0)} tokens`,
    );
  }
  if (agent.durationMs) {
    items.push(formatDuration(agent.durationMs));
  }
  const runtime = executorMetaValue(agent.executor, agent.workspace);
  if (runtime) {
    items.push(runtime);
  }
  return items;
}

function AgentDetails({
  agent,
  failed,
  runDetails,
  display,
  onOpenTranscript,
}: {
  agent: AgentSurfaceItem;
  failed: boolean;
  runDetails?: AgentRunProjectionState;
  display: ReturnType<typeof agentDisplayState>;
  onOpenTranscript: () => void;
}) {
  const updated = new Date(agent.updatedAt);
  const active = isAgentActive(agent.status);
  const ids = [
    ["Files", workspaceMetaValue(agent.workspace)],
    ["Runtime", executorMetaValue(agent.executor, agent.workspace)],
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
        {agent.reason ? <span>{agentReasonText(agent, display)}</span> : null}
        {agent.toolName ? <span>Tool {agent.toolName}</span> : null}
        {agent.runId ? (
          <button
            type="button"
            onClick={onOpenTranscript}
            className="ml-auto inline-flex items-center gap-1 rounded-control border border-border bg-surface px-2 py-1 font-medium text-text-secondary hover:bg-surface-muted hover:text-text"
          >
            Open transcript
            <ChevronRight className="size-3" />
          </button>
        ) : null}
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

function agentReasonText(
  agent: AgentSurfaceItem,
  display: ReturnType<typeof agentDisplayState>,
) {
  if (agent.status.trim().toLowerCase() === "interrupted") {
    return agentInterruptionPresentation(agent.reason).detail ?? display.label;
  }
  if (display.tone === "warning") {
    return display.label;
  }
  return `Reason ${statusLabel(agent.reason ?? "")}`;
}

function AgentExecutionMeta({ agent }: { agent: AgentSurfaceItem }) {
  const items = [
    executorMetaValue(agent.executor, agent.workspace),
    workspaceMetaValue(agent.workspace),
    agent.transport ? `connection ${statusLabel(agent.transport)}` : undefined,
    agent.fallbackPolicy
      ? `policy ${statusLabel(agent.fallbackPolicy)}`
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
  const recent = events
    .filter(
      (event) =>
        event.type !== "agent_live_event:output_delta" &&
        event.type !== "agent_live_event:thinking_delta",
    )
    .slice(-8)
    .reverse();
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
        <AgentTranscriptCard transcript={transcript} active={active} />
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

function AgentTranscriptCard({
  transcript,
  active,
}: {
  transcript: AgentLiveTranscript;
  active: boolean;
}) {
  const [open, setOpen] = useState(active);
  return (
    <div className="mb-2">
      <button
        type="button"
        className="group -ml-1 flex w-full min-w-0 items-center gap-2 rounded-[6px] px-1 py-1 text-left transition hover:text-text"
        aria-expanded={open}
        aria-label={`${transcript.label}: ${transcript.preview}`}
        onClick={() => setOpen((value) => !value)}
      >
        <span className="inline-flex shrink-0 items-center gap-1.5 text-[11px] font-semibold text-text-secondary">
          {active && transcript.kind === "thinking" ? (
            <span className="size-1.5 animate-pulse rounded-full bg-accent" />
          ) : null}
          {transcript.label}
        </span>
        <span className="min-w-0 flex-1 truncate text-[11px] leading-4 text-text-muted">
          {transcript.preview}
        </span>
        <ChevronRight
          className={cn(
            "size-3.5 shrink-0 text-text-muted transition-transform group-hover:text-text-secondary",
            open && "rotate-90",
          )}
        />
      </button>
      <div
        className={cn(
          "grid transition-[grid-template-rows] duration-200 ease-out",
          open ? "grid-rows-[1fr]" : "grid-rows-[0fr]",
        )}
      >
        <div className="min-h-0 overflow-hidden">
          <div className="ml-[5px] max-h-44 overflow-y-auto border-l border-border/70 py-1.5 pl-3 pr-1 text-xs leading-5 text-text-secondary">
            <p className="whitespace-pre-wrap break-words font-normal">
              {transcript.detail}
            </p>
          </div>
        </div>
      </div>
    </div>
  );
}

type AgentLiveTranscript = {
  kind: "thinking" | "output";
  label: string;
  preview: string;
  detail: string;
};

function agentLiveTranscript(
  events: NonNullable<AgentSurfaceItem["events"]>,
): AgentLiveTranscript | null {
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
  const kind: AgentLiveTranscript["kind"] =
    latest.type === "agent_live_event:thinking_delta" ? "thinking" : "output";
  const detail = textEvents
    .map((event) => event.detail)
    .filter((detail): detail is string => Boolean(detail))
    .join("\n")
    .slice(-4000);
  return {
    kind,
    label: kind === "thinking" ? "Thinking" : "Output",
    preview: firstNonEmptyLine(detail),
    detail,
  };
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

function AgentTranscriptWorkspace({
  agent,
  agents,
  rootRunId,
  details,
  onSelectAgent,
  onClose,
}: {
  agent: AgentSurfaceItem;
  agents: AgentSurfaceItem[];
  rootRunId: string | null;
  details?: AgentRunProjectionState;
  onSelectAgent: (agentId: string) => void;
  onClose: () => void;
}) {
  const projection = details?.projection;
  const transcript = projection?.transcript ?? [];
  const events = projection?.events ?? [];
  const display = agentDisplayState(agent);
  const title = agent.description || agent.agentType || "Subagent";
  const awaitingProjection = Boolean(agent.runId && !details);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    <div className="fixed inset-0 z-50 bg-bg">
      <section
        role="dialog"
        aria-modal="true"
        aria-label={`${title} transcript`}
        className="relative flex h-full w-full flex-col overflow-hidden bg-bg"
      >
        <header className="flex min-h-16 shrink-0 items-center gap-3 border-b border-border bg-surface px-4 sm:px-5">
          <button
            type="button"
            onClick={onClose}
            className="inline-flex size-9 shrink-0 items-center justify-center rounded-control border border-border bg-bg text-text-muted transition hover:bg-surface-muted hover:text-text"
            aria-label="Back to main conversation"
            title="Back to main conversation"
          >
            <ChevronRight className="size-4 rotate-180" />
          </button>
          <span
            className={cn(
              "hidden size-9 shrink-0 items-center justify-center rounded-control border sm:flex",
              isAgentActive(agent.status)
                ? "border-accent/25 bg-accent/10 text-accent"
                : display.tone === "danger"
                  ? "border-danger/20 bg-danger/10 text-danger"
                  : "border-border bg-surface-muted text-text-secondary",
            )}
          >
            <Bot className="size-4" />
          </span>
          <div className="min-w-0 flex-1">
            <div className="flex min-w-0 items-center gap-1.5 text-[11px] text-text-muted">
              <span className="shrink-0">Main conversation</span>
              <ChevronRight className="size-3 shrink-0" />
              <span className="truncate">
                {agentParentLabel(agent, agents, rootRunId)}
              </span>
            </div>
            <div className="mt-0.5 flex min-w-0 items-center gap-2">
              <h2 className="truncate text-sm font-semibold text-text">
                {title}
              </h2>
              <StatusPill
                status={agent.status}
                active={isAgentActive(agent.status)}
                label={display.label}
                tone={display.tone}
              />
            </div>
          </div>
          <div className="hidden items-center gap-2 text-xs text-text-muted md:flex">
            <span>{transcript.length} messages</span>
            <span aria-hidden="true">·</span>
            <span className="font-mono">
              {shortRunId(agent.runId ?? agent.agentId)}
            </span>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="inline-flex size-9 items-center justify-center rounded-control text-text-muted hover:bg-surface-muted hover:text-text"
            aria-label="Close agent transcript"
          >
            <X className="size-4" />
          </button>
        </header>

        <div className="grid min-h-0 flex-1 md:grid-cols-[240px_minmax(0,1fr)] xl:grid-cols-[268px_minmax(0,1fr)]">
          <AgentRunNavigator
            agents={agents}
            selectedAgentId={agent.agentId}
            rootRunId={rootRunId}
            onSelectAgent={onSelectAgent}
            onClose={onClose}
          />
          <div className="grid min-h-0 min-w-0 flex-1 lg:grid-cols-[minmax(0,1fr)_300px]">
            <main className="min-h-0 overflow-y-auto px-5 py-6 sm:px-8">
              {(awaitingProjection || details?.loading) && !projection ? (
                <div className="flex h-full min-h-56 items-center justify-center gap-2 text-sm text-text-muted">
                  <Loader2 className="size-4 animate-spin" />
                  Loading canonical run view
                </div>
              ) : details?.error && !projection ? (
                <div className="rounded-card border border-danger/20 bg-danger/5 p-4 text-sm text-danger">
                  {details.error}
                </div>
              ) : transcript.length ? (
                <div className="mx-auto max-w-3xl space-y-5">
                  {transcript.map((item) => (
                    <TranscriptMessage
                      key={`${item.item_seq}:${item.role}`}
                      item={item}
                    />
                  ))}
                </div>
              ) : (
                <div className="mx-auto max-w-3xl">
                  <div className="rounded-card border border-warning/20 bg-warning/5 p-4">
                    <p className="text-sm font-medium text-text">
                      Canonical conversation is not available yet
                    </p>
                    <p className="mt-1 text-xs leading-5 text-text-muted">
                      Live run evidence is shown below. Activity events are not
                      presented as a substitute for missing conversation
                      history.
                    </p>
                  </div>
                  <div className="mt-5 space-y-2">
                    {events.length ? (
                      events.map((event, index) => (
                        <RunProjectionEventRow
                          key={`${eventType(event)}:${index}`}
                          event={event}
                        />
                      ))
                    ) : (
                      <p className="text-sm text-text-muted">
                        No run evidence has arrived.
                      </p>
                    )}
                  </div>
                </div>
              )}
            </main>

            <aside className="min-h-0 overflow-y-auto border-t border-border bg-surface/70 p-4 lg:border-l lg:border-t-0">
              <AgentTranscriptSummary
                agent={agent}
                projection={projection}
                transcriptCount={transcript.length}
              />
            </aside>
          </div>
        </div>
      </section>
    </div>
  );
}

function AgentRunNavigator({
  agents,
  selectedAgentId,
  rootRunId,
  onSelectAgent,
  onClose,
}: {
  agents: AgentSurfaceItem[];
  selectedAgentId: string;
  rootRunId: string | null;
  onSelectAgent: (agentId: string) => void;
  onClose: () => void;
}) {
  const rows = agentHierarchyRows(agents, rootRunId);
  return (
    <aside className="min-h-0 overflow-y-auto border-b border-border bg-surface/65 p-3 md:border-b-0 md:border-r">
      <div className="mb-2 px-2 pt-1">
        <p className="text-[10px] font-semibold uppercase tracking-[0.1em] text-text-muted">
          Run navigator
        </p>
        <p className="mt-1 text-xs leading-5 text-text-secondary">
          Switch conversations without losing run context.
        </p>
      </div>
      <nav aria-label="Agent run transcripts" className="space-y-1">
        <button
          type="button"
          onClick={onClose}
          className="flex w-full items-center gap-2 rounded-[8px] px-2.5 py-2 text-left text-xs font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text"
        >
          <span className="flex size-6 shrink-0 items-center justify-center rounded-full border border-border bg-bg text-text-muted">
            <Activity className="size-3" />
          </span>
          <span className="min-w-0 flex-1 truncate">Main conversation</span>
        </button>
        <div className="my-2 h-px bg-border/70" />
        {rows.map((row) => {
          const display = agentDisplayState(row.agent);
          const active = row.agent.agentId === selectedAgentId;
          return (
            <button
              key={row.agent.agentId}
              type="button"
              onClick={() => onSelectAgent(row.agent.agentId)}
              aria-label={`${row.agent.description || row.agent.agentType || "Subagent"}, ${display.label}`}
              aria-current={active ? "page" : undefined}
              className={cn(
                "flex w-full items-start gap-2 rounded-[8px] px-2.5 py-2 text-left transition",
                active
                  ? "bg-accent/10 text-text ring-1 ring-accent/20"
                  : "text-text-secondary hover:bg-surface-muted hover:text-text",
              )}
              style={{ paddingLeft: `${10 + Math.min(row.depth, 3) * 12}px` }}
            >
              <span
                className={cn(
                  "mt-1 size-2 shrink-0 rounded-full",
                  isAgentActive(row.agent.status)
                    ? "animate-pulse bg-accent"
                    : display.tone === "success"
                      ? "bg-success"
                      : display.tone === "warning"
                        ? "bg-warning"
                        : display.tone === "danger"
                          ? "bg-danger"
                          : "bg-border-strong",
                )}
              />
              <span className="min-w-0 flex-1">
                <span className="block truncate text-xs font-medium">
                  {row.agent.description || row.agent.agentType || "Subagent"}
                </span>
                <span className="mt-0.5 block truncate text-[10px] text-text-muted">
                  {display.label}
                  {row.childCount ? ` · ${row.childCount} child` : ""}
                </span>
              </span>
            </button>
          );
        })}
      </nav>
    </aside>
  );
}

function TranscriptMessage({
  item,
}: {
  item: NonNullable<WorkSurfaceRunResponse["transcript"]>[number];
}) {
  const role = item.role.toLowerCase();
  const isAssistant = role === "assistant";
  const isTool = role === "tool";
  return (
    <article
      className={cn(
        "rounded-card border p-4",
        isAssistant
          ? "border-border bg-surface"
          : isTool
            ? "border-border bg-slate-950 text-slate-100"
            : "border-blue-200/60 bg-blue-50/55",
      )}
    >
      <div className="flex items-center justify-between gap-3">
        <span
          className={cn(
            "text-[11px] font-semibold uppercase tracking-[0.1em]",
            isTool
              ? "text-slate-400"
              : isAssistant
                ? "text-accent"
                : "text-blue-700",
          )}
        >
          {statusLabel(item.role)}
        </span>
        {item.created_at ? (
          <span
            className={cn(
              "text-[11px]",
              isTool ? "text-slate-500" : "text-text-muted",
            )}
          >
            {formatTranscriptTime(item.created_at)}
          </span>
        ) : null}
      </div>
      {item.reasoning?.trim() ? (
        <details className="mt-3 rounded-control border border-border/60 bg-bg/60 px-3 py-2 text-xs">
          <summary className="cursor-pointer font-medium text-text-muted">
            Reasoning
          </summary>
          <p className="mt-2 whitespace-pre-wrap leading-5 text-text-secondary">
            {item.reasoning}
          </p>
        </details>
      ) : null}
      <p
        className={cn(
          "mt-3 whitespace-pre-wrap break-words text-sm leading-6",
          isTool ? "font-mono text-[13px] text-slate-200" : "text-text",
        )}
      >
        {item.content || "(empty message)"}
      </p>
    </article>
  );
}

function AgentTranscriptSummary({
  agent,
  projection,
  transcriptCount,
}: {
  agent: AgentSurfaceItem;
  projection: WorkSurfaceRunResponse | null | undefined;
  transcriptCount: number;
}) {
  const facts = [
    ["Messages", String(transcriptCount)],
    ["Tools", String(agent.totalToolCalls ?? 0)],
    [
      "Tokens",
      String(
        (agent.totalPromptTokens ?? 0) + (agent.totalCompletionTokens ?? 0),
      ),
    ],
    ["Duration", agent.durationMs ? formatDuration(agent.durationMs) : "Live"],
  ];
  return (
    <div className="space-y-5">
      <section>
        <h3 className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
          Run summary
        </h3>
        <div className="mt-3 grid grid-cols-2 gap-2">
          {facts.map(([label, value]) => (
            <div
              key={label}
              className="rounded-control border border-border bg-bg p-2.5"
            >
              <p className="text-[10px] uppercase tracking-[0.08em] text-text-muted">
                {label}
              </p>
              <p className="mt-1 truncate text-sm font-semibold text-text">
                {value}
              </p>
            </div>
          ))}
        </div>
      </section>
      {projection ? <AgentRunBindingSummary projection={projection} /> : null}
      {projection?.transcriptWarning ? (
        <section className="rounded-control border border-warning/20 bg-warning/5 p-3 text-xs leading-5 text-text-muted">
          {projection.transcriptWarning}
        </section>
      ) : null}
      {agent.resultSummary || agent.error ? (
        <section>
          <h3 className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
            Result
          </h3>
          <p className="mt-2 whitespace-pre-wrap text-xs leading-5 text-text-secondary">
            {agent.error ?? agent.resultSummary}
          </p>
        </section>
      ) : null}
    </div>
  );
}

function formatTranscriptTime(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? value
    : date.toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      });
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
    rawWs && typeof rawWs === "object" && !Array.isArray(rawWs)
      ? (rawWs as Partial<WorkspaceBinding>)
      : null;
  const executor: ExecutorBindingLike =
    rawEx && typeof rawEx === "object" && !Array.isArray(rawEx)
      ? (rawEx as Partial<ExecutorBinding>)
      : null;
  const files = workspaceMetaValue(workspace);
  const runtime = executorMetaValue(executor, workspace);
  if (
    !files &&
    !runtime &&
    !projection.transport &&
    !projection.fallbackPolicy
  ) {
    return null;
  }
  const items = [
    runtime
      ? {
          label: "Runtime",
          value: runtime,
          detail: executorDetail(executor),
        }
      : null,
    files
      ? {
          label: "Files",
          value: files,
          detail: workspace?.cwd ?? workspace?.authority ?? undefined,
        }
      : null,
    projection.transport
      ? { label: "Connection", value: statusLabel(projection.transport) }
      : null,
    projection.fallbackPolicy
      ? { label: "Policy", value: statusLabel(projection.fallbackPolicy) }
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
              : event.tone === "warning"
                ? "bg-warning"
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

function RunBlockedBanner({
  blocked,
}: {
  blocked: WorkSurfaceState["blocked"];
}) {
  if (!blocked) {
    return null;
  }
  const runtime = executorMetaValue(blocked.executor, blocked.workspace);
  const files = workspaceMetaValue(blocked.workspace);
  const fallback = blocked.fallbackPolicy
    ? `policy ${statusLabel(blocked.fallbackPolicy)}`
    : undefined;
  const details = [
    runtime,
    files,
    blocked.transport
      ? `connection ${statusLabel(blocked.transport)}`
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
  const skipped = tool.status === "skipped";
  const [expanded, setExpanded] = useState(running || failed || cancelled);
  const finishedAt = tool.finishedAt ?? tool.startedAt;
  const displayResult = toolResultForDisplay(tool);

  useEffect(() => {
    if (running || failed || cancelled) {
      setExpanded(true);
    }
  }, [cancelled, failed, running]);

  return (
    <section className="relative pl-9">
      <div
        className={cn(
          "absolute left-[9px] top-3 z-10 flex size-4 items-center justify-center rounded-full border bg-bg",
          running
            ? "border-accent text-accent"
            : cancelled
              ? "border-text-muted text-text-muted"
              : skipped
                ? "border-border text-text-muted"
                : failed
                  ? "border-danger text-danger"
                  : "border-success text-success",
        )}
      >
        {running ? (
          <span className="size-2 animate-pulse rounded-full bg-accent" />
        ) : cancelled ? (
          <Pause className="size-3" />
        ) : skipped ? (
          <Circle className="size-3" />
        ) : failed ? (
          <AlertTriangle className="size-3" />
        ) : (
          <CheckCircle2 className="size-3" />
        )}
      </div>
      <div
        className={cn(
          "overflow-hidden rounded-card border bg-bg shadow-sm transition-colors",
          running
            ? "border-accent/35 bg-accent/5"
            : cancelled
              ? "border-border/70 bg-surface-muted/35"
              : skipped
                ? "border-border/70 bg-surface-muted/25"
                : failed
                  ? "border-danger/25 bg-danger/5"
                  : "border-border/70",
        )}
      >
        <button
          type="button"
          className="group flex w-full items-start gap-2 p-3 text-left transition-colors hover:bg-surface/70"
          aria-expanded={expanded}
          aria-label={`${expanded ? "Collapse" : "Expand"} ${tool.tool} details`}
          onClick={() => setExpanded((value) => !value)}
        >
          <Terminal className="mt-0.5 size-4 shrink-0 text-text-muted" />
          <span className="min-w-0 flex-1">
            <span className="flex items-center gap-2">
              <span className="min-w-0 flex-1 truncate font-mono text-xs font-semibold text-text">
                {tool.tool}
              </span>
              <time className="shrink-0 text-[10px] text-text-muted">
                {formatEventTime(finishedAt)}
              </time>
              <StatusPill status={tool.status} active={running} />
              <ChevronRight
                className={cn(
                  "size-3.5 shrink-0 text-text-muted transition-transform",
                  expanded ? "rotate-90" : "group-hover:translate-x-0.5",
                )}
              />
            </span>
            {!expanded ? <ToolCompactMeta tool={tool} /> : null}
          </span>
        </button>
        {failed ? (
          <div className="px-3 pb-3">
            <ToolFailureNotice tool={tool} />
          </div>
        ) : null}
        {expanded ? (
          <div className="border-t border-border/60 px-3 pb-3 pt-1">
            <ToolExecutionMeta tool={tool} />
            {tool.arguments ? (
              <StructuredPayload label="Args" value={tool.arguments} />
            ) : null}
            {displayResult ? (
              <StructuredPayload
                label="Result"
                value={displayResult}
                tone={failed ? "danger" : "muted"}
              />
            ) : running ? (
              <div className="mt-2 flex items-center gap-2 text-xs text-text-muted">
                <MiniLiveDots className="text-accent" />
                Executing
              </div>
            ) : null}
          </div>
        ) : null}
      </div>
    </section>
  );
}

function ToolCompactMeta({ tool }: { tool: ToolSurfaceItem }) {
  const items = [
    tool.durationMs !== undefined ? formatDuration(tool.durationMs) : undefined,
    tool.route ? statusLabel(tool.route) : undefined,
    executorMetaValue(tool.executor, tool.workspace),
  ].filter((item): item is string => Boolean(item));
  if (!items.length) {
    return null;
  }
  return (
    <span className="mt-1 flex min-w-0 items-center gap-1.5 truncate text-[10px] text-text-muted">
      {items.slice(0, 3).map((item, index) => (
        <span key={`${item}:${index}`} className="contents">
          {index > 0 ? <span aria-hidden="true">·</span> : null}
          <span className="truncate">{item}</span>
        </span>
      ))}
    </span>
  );
}

function toolResultForDisplay(tool: ToolSurfaceItem) {
  if (!tool.result) {
    return undefined;
  }
  switch (tool.errorKind) {
    case "executor_offline":
    case "transport_disconnected":
    case "fallback_disabled":
    case "workspace_executor_unavailable":
    case "workspace_path_mismatch":
      return undefined;
    default:
      return tool.result;
  }
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
  const runtime = executorMetaValue(tool.executor, tool.workspace);
  if (runtime) {
    items.push({
      label: "Runtime",
      value: runtime,
      tone: tool.executor?.status === "offline" ? "danger" : "default",
    });
  }
  const files = workspaceMetaValue(tool.workspace);
  if (files) {
    items.push({ label: "Files", value: files });
  }
  if (tool.transport) {
    items.push({ label: "Connection", value: statusLabel(tool.transport) });
  }
  if (tool.fallbackPolicy) {
    items.push({ label: "Policy", value: statusLabel(tool.fallbackPolicy) });
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
      value: toolReasonLabel(tool.errorKind),
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
    return "Execution environment offline. Reconnect it or choose another environment.";
  }
  if (tool.errorKind === "transport_disconnected") {
    return "Execution connection disconnected. Reconnect it before retrying.";
  }
  if (tool.errorKind === "approval_timeout") {
    return "Approval timed out. Review pending approvals and retry.";
  }
  if (tool.errorKind === "tool_timeout" || tool.errorKind === "timeout") {
    return "Tool timed out. Retry or narrow the command.";
  }
  if (tool.errorKind === "fallback_disabled") {
    return WORKSPACE_EXECUTION_BLOCKED_MESSAGE;
  }
  if (tool.errorKind === "workspace_executor_unavailable") {
    return WORKSPACE_EXECUTION_BLOCKED_MESSAGE;
  }
  if (tool.errorKind === "workspace_path_mismatch") {
    return "Path is outside the selected file environment. Server sandbox cannot access host paths; use a relative sandbox path or select the matching Edge workspace.";
  }
  if (tool.blocked) {
    return "Tool blocked. Resolve the execution environment before retrying.";
  }
  return undefined;
}

function toolReasonLabel(reason: string) {
  switch (reason) {
    case "workspace_executor_unavailable":
    case "fallback_disabled":
    case "workspace_path_mismatch":
      return "Needs file environment";
    case "executor_offline":
    case "transport_disconnected":
      return "Environment unavailable";
    default:
      return statusLabel(reason);
  }
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
    <div className="px-2 py-1.5">
      <div className="text-sm font-semibold tabular-nums text-text">
        {value}
      </div>
      <div className="text-[10px] font-medium uppercase tracking-[0.06em] text-text-muted">
        {label}
      </div>
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
      <p>{loading ? "Loading activity" : label}</p>
    </div>
  );
}

function StatusPill({
  status,
  active = false,
  label,
  tone,
}: {
  status: string;
  active?: boolean;
  label?: string;
  tone?: AgentDisplayTone;
}) {
  const done = isDone(status) || status === "done";
  const waiting = status === "waiting";
  const cancelled = status === "cancelled";
  const failed = status === "failed" || status === "interrupted";
  const effectiveTone =
    tone ??
    (cancelled
      ? "neutral"
      : waiting
        ? "warning"
        : failed
          ? "danger"
          : done
            ? "success"
            : active
              ? "running"
              : "neutral");
  return (
    <span
      className={cn(
        "inline-flex shrink-0 items-center gap-1 rounded-full px-2 py-0.5 text-[11px] font-medium",
        effectiveTone === "danger"
          ? "bg-danger/10 text-danger"
          : effectiveTone === "warning"
            ? "bg-warning/10 text-warning"
            : effectiveTone === "success"
              ? "bg-success/10 text-success"
              : effectiveTone === "running"
                ? "bg-accent/10 text-accent"
                : "bg-surface-muted text-text-muted",
      )}
    >
      {active ? (
        <span className="size-1.5 animate-pulse rounded-full bg-current" />
      ) : null}
      {label ?? (waiting ? "Waiting" : statusLabel(status))}
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
  const waiting = status === "waiting";
  const cancelled = status === "cancelled";
  const failed = status === "failed" || status === "interrupted";
  const className = cn(
    "shrink-0",
    small ? "size-3" : "mt-0.5 size-4",
    done
      ? "text-success"
      : running
        ? "text-accent"
        : waiting
          ? "text-warning"
          : cancelled
            ? "text-text-muted"
            : failed
              ? "text-danger"
              : "text-text-muted",
  );
  if (done) return <CheckCircle2 className={className} />;
  if (waiting) return <Pause className={className} />;
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

function taskObservedProgress(task: SessionTask, completedSubtasks: number) {
  const subtasks = task.subtasks ?? [];
  if (subtasks.length) {
    return {
      value: Math.round((completedSubtasks / subtasks.length) * 100),
      label: "Subtask completion",
    };
  }
  if (isDone(task.status)) {
    return { value: 100, label: "Completed" };
  }
  return null;
}

function formatTaskUpdatedAt(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? "recently"
    : date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function workbenchActivitySummary(state: WorkSurfaceState) {
  const parts: string[] = [];
  const openTasks = state.tasks.filter(
    (task) => !isDone(task.status) && task.status !== "cancelled",
  ).length;
  const activeAgents = state.agents.filter((agent) =>
    isAgentActive(agent.status),
  ).length;
  const activeTools = state.tools.filter(
    (tool) => tool.status === "running" && !tool.finishedAt,
  ).length;
  if (openTasks)
    parts.push(`${openTasks} open task${openTasks === 1 ? "" : "s"}`);
  if (activeAgents) {
    parts.push(`${activeAgents} active agent${activeAgents === 1 ? "" : "s"}`);
  }
  if (activeTools) {
    parts.push(`${activeTools} running tool${activeTools === 1 ? "" : "s"}`);
  }
  return parts.join(" · ");
}

function tabItemCount(tab: WorkSurfaceTab, state: WorkSurfaceState) {
  if (tab === "tasks") return state.tasks.length;
  if (tab === "agents") return state.agents.length;
  if (tab === "tools") return state.tools.length;
  return 0;
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
  if (agent.error) return true;
  const display = agentDisplayState(agent);
  if (display.tone === "danger") return true;
  if (agent.status === "interrupted" && display.tone === "warning") {
    return true;
  }
  if (agent.status === "waiting") {
    const reason = (agent.reason ?? "").trim().toLowerCase();
    return (
      reason === "executor_offline" ||
      reason === "fallback_disabled" ||
      reason === "workspace_executor_unavailable" ||
      reason === "transport_disconnected"
    );
  }
  return false;
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

function insightRecommendationCount(payload: ChatInsightsResponse | null) {
  const recommendations = payload?.reflection?.recommendations;
  return Array.isArray(recommendations) ? recommendations.length : 0;
}

function isAgentActive(status: string) {
  return ACTIVE_AGENT_SURFACE_STATUSES.has(status.trim().toLowerCase());
}

function statusLabel(status: string) {
  return status.replace(/[_-]+/g, " ");
}

function runStatusHeadline(status: string) {
  const normalized = status.trim().toLowerCase();
  switch (normalized) {
    case "running":
      return "Thinking";
    case "input-queued":
      return "Message queued";
    case "waiting":
      return "Waiting";
    case "blocked":
      return "Needs attention";
    case "paused":
      return "Paused";
    case "cancelling":
      return "Stopping";
    default: {
      const label = statusLabel(status).trim();
      return label ? label.charAt(0).toUpperCase() + label.slice(1) : "Active";
    }
  }
}

function isTerminalRunStatus(status: string) {
  return ["completed", "failed", "cancelled", "stopped"].includes(
    status.trim().toLowerCase(),
  );
}

function workspaceMetaValue(workspace: WorkspaceBindingLike) {
  if (!workspace || workspace.kind === "none") {
    return undefined;
  }
  return (
    workspace.cwd ?? workspace.display_name ?? workspaceDisplayName(workspace)
  );
}

function executorMetaValue(
  executor: ExecutorBindingLike,
  workspace?: WorkspaceBindingLike,
) {
  if (!executor || executor.executor_id === "server-control-plane") {
    return undefined;
  }
  if (executor.display_name === "Astra") {
    return undefined;
  }
  if (
    executor.kind === "server_local" &&
    (!workspace || workspace.kind === "none")
  ) {
    return undefined;
  }
  return executorDisplayName(executor);
}

function workspaceDisplayName(workspace: WorkspaceBindingLike) {
  return (
    workspace?.display_name ??
    (workspace?.kind ? statusLabel(workspace.kind) : "Files pending")
  );
}

function executorDisplayName(executor: ExecutorBindingLike) {
  return (
    executor?.display_name ??
    (executor?.kind ? statusLabel(executor.kind) : "Runtime pending")
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

function firstNonEmptyLine(text: string) {
  const line = text.trim().split(/\r?\n/).find(Boolean) ?? "";
  if (!line) {
    return "No details yet";
  }
  return line.length > 96 ? `${line.slice(0, 93)}...` : line;
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
