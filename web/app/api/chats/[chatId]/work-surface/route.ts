import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import {
  RuntimeClientError,
  runtimeErrorDetail,
} from "@/lib/runtime-client";
import {
  parseWorkSurfaceEvent,
  type WorkSurfaceEvent,
} from "@/lib/work-surface";
import { AstraApiError } from "@astra/sdk";
import type { ExecutorBinding, WorkspaceBinding } from "@astra/sdk";
import { pendingRunApproval, type PendingRunApproval } from "@/lib/api/run-approval";

type RuntimeRunProjectionResponse = {
  run_id?: string;
  session_id?: string;
  status?: string | null;
  cancellation_origin?: "user" | "runtime" | "unverified";
  workspace?: WorkspaceBinding | null;
  executor?: ExecutorBinding | null;
  transport?: string | null;
  fallback_policy?: string | null;
  recent_events?: Array<Record<string, unknown>>;
};

type RuntimeSessionRunNode = {
  run_id: string;
  parent_run_id?: string | null;
  root_run_id?: string | null;
  depth: number;
  agent_id?: string | null;
  agent_name?: string | null;
  status: string;
  waiting_for?: string | null;
  error_message?: string | null;
  total_tool_calls: number;
  created_at: string;
  updated_at: string;
};

type RuntimeSessionRunTreeResponse = {
  session_id?: string;
  truncated?: boolean;
  runs?: RuntimeSessionRunNode[];
};

const WORK_SURFACE_RECENT_EVENT_LIMIT = 400;
const WORK_SURFACE_RUN_TREE_LIMIT = 400;

function bindingProjectionEvent(
  projection: RuntimeRunProjectionResponse | null,
): WorkSurfaceEvent | null {
  if (!projection?.workspace && !projection?.executor) {
    return null;
  }
  return {
    type: "binding_projection",
    source: "durable_run_projection",
    run_id: projection.run_id,
    session_id: projection.session_id,
    status: projection.status,
    workspace: projection.workspace ?? undefined,
    executor: projection.executor ?? undefined,
    transport: projection.transport ?? undefined,
    fallback_policy: projection.fallback_policy ?? undefined,
  };
}

function runTimestamp(node: RuntimeSessionRunNode) {
  const value = Date.parse(node.updated_at || node.created_at);
  return Number.isFinite(value) ? value : 0;
}

export function selectWorkSurfaceRootRun(
  tree: RuntimeSessionRunTreeResponse | null,
  preferredRunId: string | null,
) {
  const roots = (tree?.runs ?? []).filter(
    (node) => node.depth === 0 || !node.parent_run_id,
  );
  if (preferredRunId) {
    const preferred = roots.find((node) => node.run_id === preferredRunId);
    if (preferred) return preferred;
  }
  return roots.sort((left, right) => runTimestamp(right) - runTimestamp(left))[0] ?? null;
}

export function runTreeAgentProjections(
  tree: RuntimeSessionRunTreeResponse | null,
  rootRunId: string | null,
): WorkSurfaceEvent[] {
  if (!rootRunId) return [];
  return (tree?.runs ?? [])
    .filter(
      (node) =>
        Boolean(node.agent_id) &&
        (node.root_run_id === rootRunId || node.parent_run_id === rootRunId),
    )
    .sort((left, right) => runTimestamp(left) - runTimestamp(right))
    .flatMap((node) => {
      if (!node.agent_id) return [];
      return {
        type: "agent_projection" as const,
        source: "durable_run_tree",
        agent_id: node.agent_id,
        run_id: node.run_id,
        parent_run_id: node.parent_run_id ?? undefined,
        description: node.agent_name ?? node.agent_id ?? undefined,
        status: node.status,
        reason: node.waiting_for ?? undefined,
        error: node.error_message ?? undefined,
        total_tool_calls: node.total_tool_calls,
        timestamp_epoch_ms: runTimestamp(node),
      } satisfies WorkSurfaceEvent;
    });
}

export async function GET(
  _request: Request,
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

  const sessionId = chat.session?.backendSessionId ?? null;
  const activeRunId = chat.activeRun?.runId ?? null;
  if (!sessionId && !activeRunId) {
    return NextResponse.json({
      sessionId: null,
      runId: activeRunId,
      workBinding: null,
      taskGraph: null,
      events: [],
      pendingApprovals: [],
      pendingApprovalsComplete: false,
      generatedAt: new Date().toISOString(),
    });
  }

  try {
    const runtime = auth.client;
    const warnings: string[] = [];
    const runTree = sessionId
      ? await runtime
          .get<RuntimeSessionRunTreeResponse>(
            `/sessions/${encodeURIComponent(sessionId)}/runs?limit=${WORK_SURFACE_RUN_TREE_LIMIT}`,
            {
              auth: "required",
              operation: "load durable session run tree for web work surface",
            },
          )
          .catch((error: unknown) => {
            warnings.push(
              `Agent history is temporarily unavailable: ${runtimeErrorDetail(error)}`,
            );
            return null;
          })
      : null;
    if (runTree?.truncated) {
      warnings.push(
        `Agent history reached the ${WORK_SURFACE_RUN_TREE_LIMIT}-run display limit.`,
      );
    }
    // Conversation selection owns the root after the active run settles.
    // The display tree may be truncated, so use it only as a recovery hint.
    const needsSelectedRoot = !activeRunId && Boolean(sessionId) &&
      (!runTree || runTree.truncated !== false || (runTree.runs ?? []).some(node =>
        node.status === "waiting" && node.waiting_for === "tool_approval"));
    let selectedRootId: string | null = null;
    let selectedRootKnown = !needsSelectedRoot;
    if (needsSelectedRoot && sessionId) {
      try {
        selectedRootId = (await runtime.sdk.getSessionConversation(sessionId, 1)).selected_root?.run_id ?? null;
        selectedRootKnown = Boolean(selectedRootId);
        if (!selectedRootKnown) warnings.push("The current conversation run could not be verified.");
      } catch (error) {
        warnings.push(`Approval recovery is temporarily unavailable: ${runtimeErrorDetail(error)}`);
      }
    }
    const rootRun = selectWorkSurfaceRootRun(runTree, activeRunId ?? selectedRootId);
    const runId = activeRunId ?? selectedRootId ?? rootRun?.run_id ?? null;
    const projection = runId
      ? await runtime
          .get<RuntimeRunProjectionResponse>(
            `/chat/runs/${encodeURIComponent(runId)}/projection?recent_limit=${WORK_SURFACE_RECENT_EVENT_LIMIT}`,
            { auth: "required", operation: "load active run projection for web work surface" },
          )
          .catch((error: unknown) => {
            warnings.push(`Run activity is temporarily unavailable: ${runtimeErrorDetail(error)}`);
            return null;
          })
      : null;
    const resolvedSessionId = sessionId ?? projection?.session_id ?? null;
    const root = runTree?.runs?.find(node => node.run_id === runId && !node.parent_run_id);
    const rootStatus = projection?.status ?? root?.status;
    const projectionMatches = projection?.run_id === runId && projection?.session_id === resolvedSessionId &&
      typeof projection?.status === "string" && Boolean(projection.status);
    const cancellationOrigin = projection?.cancellation_origin;
    const statusConsistent = !root || !projection?.status || root.status === projection.status ||
      ![root.status, projection.status].some(status => status === "cancelled" || status === "completed");
    const cancellationKnown = rootStatus !== "cancelled" ||
      (projection?.run_id === runId && projection.status === "cancelled" &&
       (cancellationOrigin === "user" || cancellationOrigin === "runtime" || cancellationOrigin === "unverified"));
    const rootClosed = rootStatus === "completed" ||
      (rootStatus === "cancelled" && cancellationOrigin === "user");
    const treeComplete = runTree?.session_id === resolvedSessionId && runTree.truncated === false &&
      Array.isArray(runTree.runs) && runTree.runs.every(node =>
        typeof node.run_id === "string" && Boolean(node.run_id.trim()) &&
        typeof node.status === "string" && Number.isInteger(node.depth) &&
        (node.status !== "waiting" || typeof node.waiting_for === "string") &&
        (!node.parent_run_id || Boolean(node.root_run_id))) &&
      runTree.runs.some(node => node.run_id === runId && !node.parent_run_id);
    const canRecover = Boolean(runId && projectionMatches && statusConsistent && cancellationKnown &&
      selectedRootKnown && resolvedSessionId);
    const needsApprovals = !treeComplete || (runTree?.runs ?? []).some(node =>
      node.status === "waiting" && node.waiting_for === "tool_approval" &&
      (node.run_id === runId || node.root_run_id === runId));
    const pendingApprovals: PendingRunApproval[] = [];
    let pendingApprovalsComplete = Boolean(canRecover && (rootClosed || treeComplete && !needsApprovals));
    if (canRecover && !rootClosed && needsApprovals) {
      let cursor: string | null = null;
      const seen = new Set<string>();
      try {
        do {
          const suffix: string = cursor ? `?cursor=${encodeURIComponent(cursor)}` : "";
          const page: {approvals?: unknown; next_cursor?: unknown} = await runtime.get(
            `/chat/runs/${encodeURIComponent(runId!)}/approvals${suffix}`,
            { auth: "required", operation: "recover pending run approvals" },
          );
          if (!Array.isArray(page.approvals) || !(page.next_cursor === null ||
              typeof page.next_cursor === "string" && page.next_cursor.length > 0)) {
            throw new Error("Approval recovery returned an invalid page.");
          }
          for (const value of page.approvals) {
            const approval = pendingRunApproval(value);
            if (!approval || approval.sessionId !== resolvedSessionId) {
              throw new Error("Approval recovery returned an invalid request.");
            }
            pendingApprovals.push(approval);
          }
          cursor = page.next_cursor;
          if (cursor && seen.has(cursor)) throw new Error("Approval recovery returned a repeated cursor.");
          if (cursor) seen.add(cursor);
        } while (cursor);
        pendingApprovalsComplete = true;
      } catch (error) {
        warnings.push(`Approval recovery is temporarily unavailable: ${runtimeErrorDetail(error)}`);
      }
    }
    const workBinding = resolvedSessionId
      ? await runtime.sdk.getWorkSessionBinding(resolvedSessionId).catch((error: unknown) => {
          if (error instanceof AstraApiError && error.status === 404) return null;
          warnings.push(
            `Work plan is temporarily unavailable: ${runtimeErrorDetail(error)}`,
          );
          return null;
        })
      : null;
    const taskGraph = workBinding
      ? await runtime.sdk
          .getWorkTaskGraph(workBinding.work_id, workBinding.branch_id, {
            cursor: {
              graph_revision: workBinding.graph_revision,
              item_offset: 0,
              dependency_offset: 0,
            },
            itemLimit: 8,
            dependencyLimit: 128,
          })
          .catch((error: unknown) => {
            warnings.push(
              `Work plan is temporarily unavailable: ${runtimeErrorDetail(error)}`,
            );
            return null;
          })
      : null;
    const bindingProjection = bindingProjectionEvent(projection);
    const recentEvents = (projection?.recent_events ?? [])
      .map(parseWorkSurfaceEvent)
      .filter((event): event is WorkSurfaceEvent => event !== null);
    const events = [
      ...(bindingProjection ? [bindingProjection] : []),
      ...recentEvents,
      ...runTreeAgentProjections(runTree, runId),
    ];

    return NextResponse.json({
      sessionId: resolvedSessionId,
      runId,
      status: projection?.status ?? (rootRun?.run_id === runId ? rootRun.status : null),
      workspace: projection?.workspace ?? null,
      executor: projection?.executor ?? null,
      transport: projection?.transport ?? null,
      fallbackPolicy: projection?.fallback_policy ?? null,
      workBinding,
      taskGraph,
      events,
      pendingApprovals,
      pendingApprovalsComplete,
      warnings,
      generatedAt: new Date().toISOString(),
    });
  } catch (error) {
    if (error instanceof RuntimeClientError) {
      return NextResponse.json(
        { error: runtimeErrorDetail(error) },
        { status: error.status || 502 },
      );
    }
    return NextResponse.json(
      {
        error:
          error instanceof Error
            ? error.message
            : "Failed to load activity.",
      },
      { status: 502 },
    );
  }
}
