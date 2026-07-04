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

export function runWaitingStatusMessage(reason: string, blocked: boolean) {
  switch (reason) {
    case "executor_offline":
      return "Run paused because the execution environment is offline. Reconnect it or choose another environment.";
    case "transport_disconnected":
      return "Run paused because the execution connection disconnected. Reconnect it before retrying.";
    case "fallback_disabled":
      return "Run paused because this request needs a file or command environment. Connect one or choose a sandbox, then retry.";
    case "workspace_executor_unavailable":
      return "Run paused because this request needs a file or command environment. Connect one or choose a sandbox, then retry.";
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
      return "This request needs a file or command environment. Connect one or choose a sandbox, then retry.";
    case "workspace_executor_unavailable":
      return "This request needs a file or command environment. Connect one or choose a sandbox, then retry.";
    case "approval_timeout":
      return "Approval timed out. Review the pending approval and retry the tool.";
    case "workspace_path_mismatch":
      return "The referenced path is outside the selected file environment. Choose the environment that contains it or use a path inside the current one.";
    default:
      return "Tool execution is blocked. Review the execution environment before retrying.";
  }
}

function statusLabel(status: string) {
  return status.trim().replace(/[_-]+/g, " ");
}
