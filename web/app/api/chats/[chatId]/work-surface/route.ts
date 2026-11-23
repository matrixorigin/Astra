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
    // Once the active run has settled, recover approvals only for the same
    // root the conversation and approval route select. Avoid this extra read
    // unless the complete tree actually contains a waiting approval.
    const needsSelectedRoot = !activeRunId && sessionId && runTree?.session_id === sessionId &&
      runTree.truncated === false && (runTree.runs ?? []).some(node =>
        node.status === "waiting" && node.waiting_for === "tool_approval");
    let selectedRootId: string | null = null;
    let selectedRootKnown = !needsSelectedRoot;
    if (needsSelectedRoot) {
      try {
        selectedRootId = (await runtime.sdk.getSessionConversation(sessionId, 1)).selected_root?.run_id ?? null;
        selectedRootKnown = Boolean(selectedRootId && runTree.runs?.some(node =>
          node.run_id === selectedRootId && !node.parent_run_id));
        if (!selectedRootKnown) warnings.push("The current conversation run could not be verified.");
      } catch (error) {
        warnings.push(`Approval recovery is temporarily unavailable: ${runtimeErrorDetail(error)}`);
      }
    }
    const rootRun = selectWorkSurfaceRootRun(runTree, activeRunId ?? selectedRootId);
    const runId = rootRun?.run_id ?? activeRunId;
    const projection = runId
      ? await runtime
          .get<RuntimeRunProjectionResponse>(
            `/chat/runs/${encodeURIComponent(
              runId,
            )}/projection?recent_limit=${WORK_SURFACE_RECENT_EVENT_LIMIT}`,
            {
              auth: "required",
              operation: "load active run projection for web work surface",
            },
          )
          .catch((error: unknown) => {
            warnings.push(
              `Run activity is temporarily unavailable: ${runtimeErrorDetail(
                error,
              )}`,
            );
            return null;
          })
      : null;
    const resolvedSessionId = sessionId ?? projection?.session_id ?? null;
    // The run tree is already loaded for the work surface. Only waiting
    // approvals need an additional exact frontier read; ordinary runs do not.
    const root = runTree?.runs?.find(node => node.run_id === runId && !node.parent_run_id);
    const cancellationOrigin = projection?.cancellation_origin;
    const statusConsistent = !root || !projection?.status || root.status === projection.status ||
      ![root.status, projection.status].some(status => status === "cancelled" || status === "completed");
    const cancellationKnown = root?.status !== "cancelled" ||
      (projection?.run_id === runId && projection.status === "cancelled" &&
       (cancellationOrigin === "user" || cancellationOrigin === "runtime" || cancellationOrigin === "unverified"));
    const rootClosed = root?.status === "completed" ||
      (root?.status === "cancelled" && cancellationOrigin === "user");
    const waitingRuns = !rootClosed && statusConsistent && cancellationKnown && selectedRootKnown && (activeRunId || runTree?.truncated === false) && runId && resolvedSessionId && runTree?.session_id === resolvedSessionId
      ? (runTree.runs ?? []).filter(node => node.status === "waiting" &&
          node.waiting_for === "tool_approval" &&
          (node.run_id === runId || node.root_run_id === runId))
      : [];
    const pendingApprovals: PendingRunApproval[] = [];
    let pendingApprovalsComplete = Boolean(
      runId && statusConsistent && cancellationKnown && selectedRootKnown && resolvedSessionId && runTree?.session_id === resolvedSessionId &&
      runTree.truncated === false && Array.isArray(runTree.runs) &&
      runTree.runs.every(node => typeof node.run_id === "string" && Boolean(node.run_id.trim()) &&
        typeof node.status === "string" && Number.isInteger(node.depth) &&
        (node.status !== "waiting" || typeof node.waiting_for === "string") &&
        (!node.parent_run_id || Boolean(node.root_run_id))) &&
      runTree.runs.some(node => node.run_id === runId && !node.parent_run_id),
    );
    for (let offset = 0; offset < waitingRuns.length; offset += 8) {
      const batch = waitingRuns.slice(offset, offset + 8);
      const results = await Promise.allSettled(batch.map(node => runtime.get<unknown>(
        `/chat/runs/${encodeURIComponent(node.run_id)}/approval`,
        { auth: "required", operation: "recover pending run approval" },
      )));
      results.forEach((result, index) => {
        if (result.status === "rejected") {
          pendingApprovalsComplete = false;
          warnings.push(`Approval recovery is temporarily unavailable: ${runtimeErrorDetail(result.reason)}`);
          return;
        }
        const approval = pendingRunApproval(result.value);
        if (approval && approval.runId === batch[index].run_id && approval.sessionId === resolvedSessionId) {
          pendingApprovals.push(approval);
        } else if (result.value !== null) {
          pendingApprovalsComplete = false;
          warnings.push("Approval recovery returned an invalid request.");
        }
      });
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
      status: rootRun?.status ?? projection?.status ?? null,
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
