import {
  EXECUTION_BOUNDARY_WAIT_REASONS,
  isExecutionBoundaryWait,
  extractWaitingReason,
  extractBlockedReason,
  projectRunWaitingState,
  type RunWaitingProjection,
} from "@astra/sdk";

export {
  EXECUTION_BOUNDARY_WAIT_REASONS,
  isExecutionBoundaryWait,
  extractWaitingReason,
  extractBlockedReason,
  projectRunWaitingState,
};

export type { RunWaitingProjection };

export const WORKSPACE_EXECUTION_WAITING_MESSAGE =
  "Run paused because this request needs a file environment that can execute tools. Choose an available file environment or managed runtime, then retry.";

export const WORKSPACE_EXECUTION_BLOCKED_MESSAGE =
  "This request needs a file environment that can execute tools. Choose an available file environment or managed runtime, then retry.";

export function runWaitingStatusMessage(reason: string, blocked: boolean) {
  switch (reason) {
    case "executor_offline":
      return "Run paused because the execution environment is offline. Reconnect it or choose another environment.";
    case "transport_disconnected":
      return "Run paused because the execution connection disconnected. Reconnect it before retrying.";
    case "fallback_disabled":
    case "workspace_executor_unavailable":
      return WORKSPACE_EXECUTION_WAITING_MESSAGE;
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
      return "Execution environment is offline. Reconnect it or choose another environment.";
    case "transport_disconnected":
      return "Execution connection disconnected. Reconnect it or retry after it recovers.";
    case "fallback_disabled":
    case "workspace_executor_unavailable":
      return WORKSPACE_EXECUTION_BLOCKED_MESSAGE;
    case "approval_timeout":
      return "Approval timed out. Review the pending approval and retry the tool.";
    case "workspace_path_mismatch":
      return "The referenced path is outside the selected file environment. If you selected Server sandbox, use a relative path inside it; for host paths like ~/project, select the matching Edge workspace.";
    default:
      return "Tool execution is blocked. Review the execution environment before retrying.";
  }
}

function statusLabel(status: string) {
  return status.trim().replace(/[_-]+/g, " ");
}
