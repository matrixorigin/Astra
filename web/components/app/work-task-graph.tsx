"use client";

import type {
  WorkTaskGraphCursorV1,
  WorkTaskGraphDependencyV1,
  WorkTaskGraphItemV2,
  WorkTaskGraphPageV2,
} from "@astra/sdk";
import {
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  Loader2,
} from "lucide-react";
import { useRouter } from "next/navigation";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  loadWorkTaskGraphPageAction,
  refreshWorkTaskGraphAction,
} from "@/app/(workspace)/works/[workId]/actions";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { cn } from "@/lib/utils/cn";
import {
  isWorkTaskOpen,
  workTaskCounts,
  workTaskNeedsAttention,
  workTaskPresentation,
  type WorkTaskTone,
} from "@/lib/work-task-presentation";

type TaskFilter = "active" | "attention" | "all";

type WorkTaskGraphProps = {
  initial: WorkTaskGraphPageV2;
  live?: boolean;
};

const LIVE_REFRESH_INTERVAL_MS = 2_000;
const QUIET_REFRESH_INTERVAL_MS = 30_000;

export function WorkTaskGraph({ initial, live = false }: WorkTaskGraphProps) {
  const [head, setHead] = useState(initial);
  const [items, setItems] = useState(initial.items.entries);
  const [dependencies, setDependencies] = useState(initial.dependencies.entries);
  const [nextCursor, setNextCursor] = useState(initial.next_cursor);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [liveError, setLiveError] = useState(false);
  const [filter, setFilter] = useState<TaskFilter>("active");
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [recentlyChanged, setRecentlyChanged] = useState<Set<string>>(new Set());
  const [consumedCursors, setConsumedCursors] = useState<Set<string>>(new Set());
  const headRef = useRef(initial);
  const liveRefreshInFlight = useRef(false);
  const pageLoadInFlight = useRef(false);
  const liveFailureCount = useRef(0);
  const changeMarkerTimer = useRef<number | null>(null);
  const router = useRouter();

  const revealTaskChanges = useCallback(
    (previous: WorkTaskGraphItemV2[], next: WorkTaskGraphItemV2[]) => {
      const changed = changedTaskKeys(previous, next);
      if (changed.size === 0) return;
      setRecentlyChanged(changed);
      if (changeMarkerTimer.current !== null) window.clearTimeout(changeMarkerTimer.current);
      changeMarkerTimer.current = window.setTimeout(() => {
        changeMarkerTimer.current = null;
        setRecentlyChanged(new Set());
      }, 1_600);
    },
    [],
  );

  const graphKey = `${initial.basis.work_id}:${initial.basis.branch_id}:${initial.basis.graph_revision}:${initial.basis.graph_manifest_hash}`;
  useEffect(() => {
    revealTaskChanges(headRef.current.items.entries, initial.items.entries);
    headRef.current = initial;
    setHead(initial);
    setItems(initial.items.entries);
    setDependencies(initial.dependencies.entries);
    setNextCursor(initial.next_cursor);
    setLoading(false);
    setError(null);
    setLiveError(false);
    liveFailureCount.current = 0;
    setExpanded(new Set());
    setConsumedCursors(new Set());
  }, [graphKey, initial, revealTaskChanges]);

  useEffect(
    () => () => {
      if (changeMarkerTimer.current !== null) window.clearTimeout(changeMarkerTimer.current);
    },
    [],
  );

  useEffect(() => {
    let cancelled = false;

    async function refreshHead() {
      if (liveRefreshInFlight.current || pageLoadInFlight.current) return;
      liveRefreshInFlight.current = true;
      try {
        const currentHead = headRef.current;
        const result = await refreshWorkTaskGraphAction({
          workId: currentHead.basis.work_id,
          branchId: currentHead.basis.branch_id,
        });
        if (cancelled) return;
        if (!result.ok) {
          liveFailureCount.current += 1;
          if (liveFailureCount.current >= 3) setLiveError(true);
          return;
        }
        const next = result.page;
        if (
          next.basis.work_id !== currentHead.basis.work_id ||
          next.basis.branch_id !== currentHead.basis.branch_id ||
          next.cursor.item_offset !== 0 ||
          next.cursor.dependency_offset !== 0
        ) {
          liveFailureCount.current = 3;
          setLiveError(true);
          return;
        }
        if (next.basis.graph_revision < currentHead.basis.graph_revision) return;
        if (next.basis.graph_revision > currentHead.basis.graph_revision) {
          revealTaskChanges(currentHead.items.entries, next.items.entries);
          headRef.current = next;
          setHead(next);
          setItems(next.items.entries);
          setDependencies(next.dependencies.entries);
          setNextCursor(next.next_cursor);
          setConsumedCursors(new Set());
          setExpanded(new Set());
          setError(null);
          liveFailureCount.current = 0;
          setLiveError(false);
          return;
        }
        if (!sameGraph(currentHead, next)) {
          liveFailureCount.current = 3;
          setLiveError(true);
          return;
        }
        const refreshed = new Map(
          next.items.entries.map((item) => [`${item.item_id}:${item.revision}`, item]),
        );
        revealTaskChanges(currentHead.items.entries, next.items.entries);
        headRef.current = next;
        setHead(next);
        setItems((current) =>
          current.map((item) => refreshed.get(`${item.item_id}:${item.revision}`) ?? item),
        );
        liveFailureCount.current = 0;
        setLiveError(false);
      } catch {
        if (!cancelled) {
          liveFailureCount.current += 1;
          if (liveFailureCount.current >= 3) setLiveError(true);
        }
      } finally {
        liveRefreshInFlight.current = false;
      }
    }

    if (live) void refreshHead();
    const timer = window.setInterval(
      () => void refreshHead(),
      live ? LIVE_REFRESH_INTERVAL_MS : QUIET_REFRESH_INTERVAL_MS,
    );
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [initial.basis.branch_id, initial.basis.work_id, live, revealTaskChanges]);

  const topology = useMemo(
    () => taskTopology(items, dependencies),
    [dependencies, items],
  );
  const counts = useMemo(() => workTaskCounts(items), [items]);
  const visible = useMemo(
    () =>
      items.filter((item) =>
        filter === "all"
          ? true
          : filter === "attention"
            ? workTaskNeedsAttention(item)
            : isWorkTaskOpen(item),
      ),
    [filter, items],
  );

  async function loadMore() {
    if (!nextCursor || loading) return;
    const requested = nextCursor;
    const requestedKey = cursorKey(requested);
    if (consumedCursors.has(requestedKey)) {
      setError("The Task Graph returned a repeated continuation. Refresh to read a coherent revision.");
      return;
    }
    setLoading(true);
    pageLoadInFlight.current = true;
    setError(null);
    try {
      const result = await loadWorkTaskGraphPageAction({
        workId: head.basis.work_id,
        branchId: head.basis.branch_id,
        cursor: requested,
      });
      if (!result.ok) {
        setError(
          result.status === 409 || result.status === 412
            ? "The plan changed while this page was open. Refresh to see the new revision."
            : result.retryable
              ? "More plan items are temporarily unavailable. You can safely retry."
              : "This Task Graph page can no longer be loaded.",
        );
        return;
      }
      const page = result.page;
      const currentHead = headRef.current;
      if (
        requested.graph_revision !== currentHead.basis.graph_revision ||
        !sameGraph(currentHead, page)
      ) {
        setError("The plan changed while this page was open. Refresh to see the new revision.");
        return;
      }
      const mergedItems = appendUniqueItems(items, page.items.entries);
      const mergedDependencies = appendUniqueDependencies(
        dependencies,
        page.dependencies.entries,
      );
      const nextKey = page.next_cursor ? cursorKey(page.next_cursor) : null;
      if (nextKey === requestedKey) {
        throw new Error("Task Graph continuation did not advance");
      }
      if (
        page.next_cursor === null &&
        (mergedItems.length !== page.items.total ||
          mergedDependencies.length !== page.dependencies.total)
      ) {
        throw new Error("Task Graph pagination ended before its declared totals");
      }
      setItems(mergedItems);
      setDependencies(mergedDependencies);
      setNextCursor(page.next_cursor);
      setConsumedCursors((current) => new Set(current).add(requestedKey));
    } catch {
      setError("The Task Graph response was inconsistent. Refresh to read a coherent revision.");
    } finally {
      pageLoadInFlight.current = false;
      setLoading(false);
    }
  }

  return (
    <Card className="p-0" aria-label="Work plan">
      <div className="flex flex-col gap-3 px-4 py-4 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-sm font-semibold text-text">Plan</h2>
          <p className="mt-1 text-xs leading-5 text-text-muted">
            Execution and verification are tracked independently.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-1.5 text-xs tabular-nums text-text-muted">
          {counts.working > 0 ? <span>{counts.working} working</span> : null}
          {counts.attention > 0 ? (
            <span>{counts.working > 0 ? "· " : ""}{counts.attention} need attention</span>
          ) : null}
          <span>{counts.working > 0 || counts.attention > 0 ? "· " : ""}{items.length}/{head.items.total} loaded</span>
          {live ? (
            <span className={cn("inline-flex items-center gap-1", liveError && "text-warning")}>
              <span aria-hidden="true">·</span>
              <Loader2 className="size-3 animate-spin" />
              {liveError ? "Reconnecting plan" : "Live"}
            </span>
          ) : null}
        </div>
      </div>

      {items.length > 0 ? (
        <>
          <div className="flex items-center gap-1 border-y border-border/70 px-3 py-2">
            <FilterButton
              active={filter === "active"}
              label="Current"
              count={counts.open}
              onClick={() => setFilter("active")}
            />
            {counts.attention > 0 ? (
              <FilterButton
                active={filter === "attention"}
                label="Needs attention"
                count={counts.attention}
                tone="warning"
                onClick={() => setFilter("attention")}
              />
            ) : null}
            <FilterButton
              active={filter === "all"}
              label="History"
              count={items.length}
              onClick={() => setFilter("all")}
            />
          </div>
          {visible.length > 0 ? (
            <ol className="divide-y divide-border/70">
              {visible.map((item) => {
                const isExpanded = expanded.has(item.item_id);
                return (
                  <TaskRow
                    key={`${item.item_id}:${item.revision}`}
                    item={item}
                    topology={topology}
                    expanded={isExpanded}
                    recentlyChanged={recentlyChanged.has(`${item.item_id}:${item.revision}`)}
                    onToggle={() =>
                      setExpanded((current) => {
                        const next = new Set(current);
                        if (next.has(item.item_id)) next.delete(item.item_id);
                        else next.add(item.item_id);
                        return next;
                      })
                    }
                  />
                );
              })}
            </ol>
          ) : (
            <p className="px-4 py-5 text-sm text-text-muted">
              No plan items currently need attention.
            </p>
          )}
        </>
      ) : (
        <p className="border-t border-border/70 px-4 py-5 text-sm leading-6 text-text-secondary">
          Astra will add tasks when the work needs explicit coordination.
        </p>
      )}

      {error ? (
        <div className="flex items-start justify-between gap-3 border-t border-danger/20 bg-danger/5 px-4 py-3">
          <p role="alert" className="text-xs leading-5 text-danger">
            {error}
          </p>
          <Button size="sm" variant="ghost" onClick={() => router.refresh()}>
            Refresh
          </Button>
        </div>
      ) : nextCursor ? (
        <div className="border-t border-border/70 px-4 py-3">
          <Button size="sm" variant="ghost" disabled={loading} onClick={() => void loadMore()}>
            {loading ? (
              <>
                <Loader2 className="size-3.5 animate-spin" /> Loading…
              </>
            ) : (
              "Show more plan items"
            )}
          </Button>
        </div>
      ) : null}
    </Card>
  );
}

type TaskTopology = {
  labels: Map<string, string>;
  predecessors: Map<string, string[]>;
  successors: Map<string, string[]>;
};

function TaskRow({
  item,
  topology,
  expanded,
  recentlyChanged,
  onToggle,
}: {
  item: WorkTaskGraphItemV2;
  topology: TaskTopology;
  expanded: boolean;
  recentlyChanged: boolean;
  onToggle: () => void;
}) {
  const state = workTaskPresentation(item);
  const tone = taskToneClasses(state.tone);
  const blockers = topology.predecessors.get(item.item_id) ?? [];
  const unblocks = topology.successors.get(item.item_id) ?? [];
  const hasDetails =
    blockers.length > 0 ||
    unblocks.length > 0 ||
    Boolean(item.execution.run) ||
    item.delivery.status !== "unreported" ||
    Boolean(item.verification.latest_check);
  return (
    <li
      className={cn(
        "px-4 py-3.5 transition-colors duration-500",
        state.needsAttention && "bg-warning/[0.025]",
        recentlyChanged && "bg-accent/[0.07]",
      )}
    >
      <div className="flex items-start gap-3">
        <span className={cn("mt-1.5 size-2 shrink-0 rounded-full", tone.dot)} />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1">
            <p className="text-sm font-medium leading-5 text-text">{item.objective}</p>
            <span className={cn("shrink-0 text-xs font-medium", tone.text)}>
              {state.label}
            </span>
          </div>
          <p className="mt-1 text-xs leading-5 text-text-secondary">
            {item.expected_result}
          </p>
          <div className="mt-1.5 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-text-muted">
            <span>{item.kind === "milestone" ? "Milestone" : "Task"}</span>
            {recentlyChanged ? <span className="font-medium text-accent">Updated</span> : null}
            {blockers.length > 0 ? <span>{blockers.length} dependencies</span> : null}
            {unblocks.length > 0 ? <span>unblocks {unblocks.length}</span> : null}
            {hasDetails ? (
              <button
                type="button"
                className="inline-flex items-center gap-1 font-medium text-text-secondary hover:text-text"
                aria-expanded={expanded}
                onClick={onToggle}
              >
                {expanded ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
                Details
              </button>
            ) : null}
          </div>
          {expanded ? (
            <TaskDetails item={item} blockers={blockers} unblocks={unblocks} labels={topology.labels} />
          ) : null}
        </div>
      </div>
    </li>
  );
}

function TaskDetails({
  item,
  blockers,
  unblocks,
  labels,
}: {
  item: WorkTaskGraphItemV2;
  blockers: string[];
  unblocks: string[];
  labels: Map<string, string>;
}) {
  const check = item.verification.latest_check;
  return (
    <dl className="mt-3 grid gap-2 rounded-control bg-surface-muted/60 px-3 py-2.5 text-xs leading-5 sm:grid-cols-[7rem_1fr]">
      {blockers.length > 0 ? (
        <>
          <dt className="font-medium text-text-muted">Blocked by</dt>
          <dd className="text-text-secondary">{blockers.map((id) => labels.get(id) ?? id).join(" · ")}</dd>
        </>
      ) : null}
      {unblocks.length > 0 ? (
        <>
          <dt className="font-medium text-text-muted">Unblocks</dt>
          <dd className="text-text-secondary">{unblocks.map((id) => labels.get(id) ?? id).join(" · ")}</dd>
        </>
      ) : null}
      {item.execution.run ? (
        <>
          <dt className="font-medium text-text-muted">Execution</dt>
          <dd className="text-text-secondary">Attempt {item.execution.run.attempt_id} · generation {item.execution.run.run_generation}</dd>
        </>
      ) : null}
      {item.delivery.status !== "unreported" ? (
        <>
          <dt className="font-medium text-text-muted">Delivery</dt>
          <dd className="text-text-secondary">
            {item.delivery.summary}
            {item.delivery.unavailable_capabilities.length > 0
              ? ` · unavailable: ${item.delivery.unavailable_capabilities.join(", ")}`
              : null}
          </dd>
        </>
      ) : null}
      {check ? (
        <>
          <dt className="font-medium text-text-muted">Latest check</dt>
          <dd className="text-text-secondary">
            {check.outcome} · {check.coverage} coverage · {check.freshness.replaceAll("_", " ")}
          </dd>
        </>
      ) : null}
    </dl>
  );
}

function FilterButton({
  active,
  label,
  count,
  tone = "neutral",
  onClick,
}: {
  active: boolean;
  label: string;
  count: number;
  tone?: "neutral" | "warning";
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-label={`${label} ${count}`}
      aria-pressed={active}
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-1.5 rounded-control px-2.5 py-1.5 text-xs font-medium transition",
        active
          ? tone === "warning"
            ? "bg-warning/10 text-warning"
            : "bg-surface-muted text-text"
          : "text-text-muted hover:bg-surface-muted hover:text-text",
      )}
    >
      {tone === "warning" ? <AlertTriangle className="size-3" /> : null}
      {label}
      <span className="tabular-nums">{count}</span>
    </button>
  );
}

function taskTopology(
  items: WorkTaskGraphItemV2[],
  dependencies: WorkTaskGraphDependencyV1[],
): TaskTopology {
  const labels = new Map(items.map((item) => [item.item_id, item.objective]));
  const predecessors = new Map<string, string[]>();
  const successors = new Map<string, string[]>();
  for (const edge of dependencies) {
    predecessors.set(edge.successor_item_id, [
      ...(predecessors.get(edge.successor_item_id) ?? []),
      edge.predecessor_item_id,
    ]);
    successors.set(edge.predecessor_item_id, [
      ...(successors.get(edge.predecessor_item_id) ?? []),
      edge.successor_item_id,
    ]);
  }
  return { labels, predecessors, successors };
}

function changedTaskKeys(
  previous: WorkTaskGraphItemV2[],
  next: WorkTaskGraphItemV2[],
): Set<string> {
  const previousById = new Map(previous.map((item) => [item.item_id, item]));
  const changed = new Set<string>();
  for (const item of next) {
    const prior = previousById.get(item.item_id);
    if (
      !prior ||
      prior.revision !== item.revision ||
      taskRuntimeSignature(prior) !== taskRuntimeSignature(item)
    ) {
      changed.add(`${item.item_id}:${item.revision}`);
    }
  }
  return changed;
}

function taskRuntimeSignature(item: WorkTaskGraphItemV2): string {
  const run = item.execution.run;
  const check = item.verification.latest_check;
  return JSON.stringify([
    item.declaration_state,
    item.execution.status,
    item.execution.terminal,
    run?.run_id ?? null,
    run?.run_generation ?? null,
    run?.last_event_idx ?? null,
    item.delivery.status,
    item.delivery.summary,
    item.delivery.blocker_kind,
    item.delivery.unavailable_capabilities,
    item.verification.status,
    check?.check_run_id ?? null,
    check?.outcome ?? null,
    check?.coverage ?? null,
    check?.freshness ?? null,
    check?.evidence_ref_count ?? null,
  ]);
}

function taskToneClasses(tone: WorkTaskTone): { dot: string; text: string } {
  switch (tone) {
    case "running":
      return { dot: "bg-accent", text: "text-accent" };
    case "warning":
      return { dot: "bg-warning", text: "text-warning" };
    case "danger":
      return { dot: "bg-danger", text: "text-danger" };
    case "success":
      return { dot: "bg-success", text: "text-success" };
    case "neutral":
      return { dot: "bg-border-strong", text: "text-text-muted" };
  }
}

function cursorKey(cursor: WorkTaskGraphCursorV1): string {
  return `${cursor.graph_revision}:${cursor.item_offset}:${cursor.dependency_offset}`;
}

function sameGraph(initial: WorkTaskGraphPageV2, page: WorkTaskGraphPageV2): boolean {
  return (
    page.basis.work_id === initial.basis.work_id &&
    page.basis.branch_id === initial.basis.branch_id &&
    page.basis.graph_revision === initial.basis.graph_revision &&
    page.basis.graph_manifest_hash === initial.basis.graph_manifest_hash &&
    page.basis.graph_item_count === initial.basis.graph_item_count &&
    page.basis.graph_edge_count === initial.basis.graph_edge_count &&
    page.basis.graph_item_count === page.items.total &&
    page.basis.graph_edge_count === page.dependencies.total &&
    page.items.total === initial.items.total &&
    page.dependencies.total === initial.dependencies.total
  );
}

function appendUniqueItems(
  current: WorkTaskGraphItemV2[],
  incoming: WorkTaskGraphItemV2[],
): WorkTaskGraphItemV2[] {
  const identities = new Set(current.map((item) => `${item.item_id}:${item.revision}`));
  for (const item of incoming) {
    if (identities.has(`${item.item_id}:${item.revision}`)) {
      throw new Error("Task Graph repeated an item");
    }
  }
  return [...current, ...incoming];
}

function appendUniqueDependencies(
  current: WorkTaskGraphDependencyV1[],
  incoming: WorkTaskGraphDependencyV1[],
): WorkTaskGraphDependencyV1[] {
  const identities = new Set(
    current.map((edge) => `${edge.predecessor_item_id}:${edge.successor_item_id}`),
  );
  for (const edge of incoming) {
    if (identities.has(`${edge.predecessor_item_id}:${edge.successor_item_id}`)) {
      throw new Error("Task Graph repeated a dependency");
    }
  }
  return [...current, ...incoming];
}
