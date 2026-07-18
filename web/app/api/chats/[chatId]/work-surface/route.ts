import { NextResponse } from "next/server";
import { requireRuntimeUser } from "@/lib/api/auth-guard";
import { getChat } from "@/lib/api/web-store";
import {
  RuntimeClientError,
  requireRuntimeClient,
  runtimeErrorDetail,
} from "@/lib/runtime-client";
import {
  parseWorkSurfaceEvent,
  type WorkSurfaceEvent,
} from "@/lib/work-surface";
import type { ExecutorBinding, WorkspaceBinding } from "@astra/sdk";

type RuntimeTodosResponse = {
  tasks?: Array<Record<string, unknown>>;
};

type RuntimeRunProjectionResponse = {
  run_id?: string;
  session_id?: string;
  status?: string | null;
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
      tasks: [],
      events: [],
      generatedAt: new Date().toISOString(),
    });
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: "required",
      operation: "load web work surface",
    });
    const warnings: string[] = [];
    const [runTree, todos] = sessionId
      ? await Promise.all([
          runtime
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
            }),
          runtime
            .get<RuntimeTodosResponse>(
              `/sessions/${encodeURIComponent(sessionId)}/todos`,
              {
                auth: "required",
                operation: "load session todos for web work surface",
              },
            )
            .catch((error: unknown) => {
              warnings.push(
                `Tasks are temporarily unavailable: ${runtimeErrorDetail(error)}`,
              );
              return { tasks: [] };
            }),
        ])
      : [null, { tasks: [] }];
    if (runTree?.truncated) {
      warnings.push(
        `Agent history reached the ${WORK_SURFACE_RUN_TREE_LIMIT}-run display limit.`,
      );
    }
    const rootRun = selectWorkSurfaceRootRun(runTree, activeRunId);
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
    const resolvedTodos = sessionId
      ? todos
      : resolvedSessionId
        ? await runtime
            .get<RuntimeTodosResponse>(
              `/sessions/${encodeURIComponent(resolvedSessionId)}/todos`,
              {
                auth: "required",
                operation: "load session todos for web work surface",
              },
            )
            .catch((error: unknown) => {
              warnings.push(
                `Tasks are temporarily unavailable: ${runtimeErrorDetail(error)}`,
              );
              return { tasks: [] };
            })
        : { tasks: [] };
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
      tasks: Array.isArray(resolvedTodos.tasks) ? resolvedTodos.tasks : [],
      events,
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
