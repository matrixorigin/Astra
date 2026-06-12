export const EXECUTION_BOUNDARY_WAIT_REASONS = new Set([
  "executor_offline",
  "transport_disconnected",
  "fallback_disabled",
  "workspace_executor_unavailable",
]);

export function isExecutionBoundaryWait(reason: string): boolean {
  return EXECUTION_BOUNDARY_WAIT_REASONS.has(reason);
}

export type RunWaitingProjection = {
  status: "waiting" | "blocked";
  waitingFor: string;
  blocked: boolean;
};

export function extractWaitingReason(event: {
  waiting_for?: string;
  reason?: string;
  error_kind?: string;
}): string {
  const raw =
    event.waiting_for ?? event.reason ?? event.error_kind ?? "waiting";
  return raw.replace(/^waiting:\s*/i, "").trim() || "waiting";
}

export function extractBlockedReason(event: {
  type?: string;
  reason?: string;
  error_kind?: string;
  blocked?: boolean;
}): string | null {
  if (event.type === "run_blocked") {
    return event.reason ?? event.error_kind ?? "blocked";
  }
  if (event.blocked) {
    return event.reason ?? event.error_kind ?? "blocked";
  }
  return null;
}

export function projectRunWaitingState(event: {
  waiting_for?: string;
  reason?: string;
  error_kind?: string;
}): RunWaitingProjection {
  const waitingFor = extractWaitingReason(event);
  const blocked = isExecutionBoundaryWait(waitingFor);
  return {
    status: blocked ? "blocked" : "waiting",
    waitingFor,
    blocked,
  };
}

export function runWaitingStatusMessage(reason: string, blocked: boolean) {
  switch (reason) {
    case "executor_offline":
      return "Run paused because the selected executor is offline. Reconnect it or choose another workspace.";
    case "transport_disconnected":
      return "Run paused because the tool transport disconnected. Reconnect the executor before retrying.";
    case "fallback_disabled":
      return "Run paused because server fallback is disabled for this workspace.";
    case "workspace_executor_unavailable":
      return "Run paused because the selected workspace is not connected to an available executor. Choose Server sandbox or a connected edge workspace.";
    case "tool_approval":
    case "approval":
      return "Waiting for tool approval.";
    case "user_resume":
      return "Run paused. Resume it to continue.";
    default: {
      const label = statusLabel(reason.replace(/^waiting:\s*/i, ""));
      return blocked
        ? `Run paused by ${label || "an execution boundary"}.`
        : `Waiting for ${label || "runtime input"}.`;
    }
  }
}

export function blockedRunMessage(reason: string) {
  switch (reason) {
    case "executor_offline":
      return "Executor is offline. Reconnect the selected edge executor or choose another workspace.";
    case "transport_disconnected":
      return "Tool transport disconnected. Reconnect the selected executor or retry after transport recovers.";
    case "fallback_disabled":
      return "Fallback is disabled for this workspace. Choose a reachable executor or a different workspace.";
    case "workspace_executor_unavailable":
      return "The selected workspace is not connected to an available executor transport. Choose Server sandbox or a connected edge workspace.";
    case "approval_timeout":
      return "Approval timed out. Review the pending approval and retry the tool.";
    case "workspace_path_mismatch":
      return "The selected workspace does not own the referenced local path. Choose a matching edge workspace or use paths inside the bound workspace.";
    default:
      return "Tool execution is blocked. Review the executor, workspace, and transport state before retrying.";
  }
}

function statusLabel(status: string) {
  return status.trim().replace(/[_-]+/g, " ");
}
