/**
 * Shared run lifecycle utilities for classifying waiting/blocked states.
 *
 * A "waiting" reason is classified as an execution boundary wait if it
 * matches one of the known infrastructure failure reasons (executor offline,
 * transport disconnected, fallback disabled, workspace executor unavailable).
 * These represent states where the run cannot proceed until the underlying
 * infrastructure recovers or the user changes configuration.
 *
 * MUST stay in sync with the Rust constants:
 *   TOOL_ERROR_KIND_EXECUTOR_OFFLINE            = "executor_offline"
 *   TOOL_ERROR_KIND_TRANSPORT_DISCONNECTED       = "transport_disconnected"
 *   TOOL_ERROR_KIND_FALLBACK_DISABLED            = "fallback_disabled"
 *   TOOL_ERROR_KIND_WORKSPACE_EXECUTOR_UNAVAILABLE = "workspace_executor_unavailable"
 */

export const EXECUTION_BOUNDARY_WAIT_REASONS = new Set([
  "executor_offline",
  "transport_disconnected",
  "fallback_disabled",
  "workspace_executor_unavailable",
]);

export function isExecutionBoundaryWait(reason: string): boolean {
  return EXECUTION_BOUNDARY_WAIT_REASONS.has(reason);
}

/**
 * Extract a normalized waiting reason from a stream event.
 *
 * Checks `waiting_for`, `reason`, and `error_kind` fields in order,
 * strips the "waiting: " prefix if present, and defaults to "waiting".
 */
export function extractWaitingReason(event: {
  waiting_for?: string | null;
  reason?: string;
  error_kind?: string;
}): string {
  const raw =
    event.waiting_for ?? event.reason ?? event.error_kind ?? "waiting";
  return raw.replace(/^waiting:\s*/i, "").trim() || "waiting";
}

/**
 * Extract a blocked reason from a stream event, or null if not blocked.
 *
 * Handles `run_blocked` event type and explicit `blocked: true` flags.
 */
export function extractBlockedReason(event: {
  type?: string;
  reason?: string;
  error_kind?: string;
  blocked?: boolean;
}): string | null {
  if (event.type?.startsWith("run_blocked_")) {
    return (
      event.reason ??
      event.error_kind ??
      event.type.slice("run_blocked_".length)
    );
  }
  if (event.blocked) {
    return event.reason ?? event.error_kind ?? "blocked";
  }
  return null;
}
