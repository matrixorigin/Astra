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
