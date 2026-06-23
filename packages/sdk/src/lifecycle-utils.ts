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

import type { ToolStatus } from "./types";

export const EXECUTION_BOUNDARY_WAIT_REASONS = new Set([
  "executor_offline",
  "transport_disconnected",
  "fallback_disabled",
  "workspace_executor_unavailable",
]);

export function isExecutionBoundaryWait(reason: string): boolean {
  return EXECUTION_BOUNDARY_WAIT_REASONS.has(reason);
}

export type ToolTerminalStatusEvent = {
  type?: string;
  status?: unknown;
  success?: boolean;
  error_kind?: string | null;
  reason?: string | null;
  cancelled?: boolean;
  skipped?: boolean;
};

export function normalizeToolStatus(status: unknown): ToolStatus | undefined {
  if (typeof status !== "string") return undefined;
  switch (status.trim().toLowerCase()) {
    case "done":
    case "completed":
      return "done";
    case "error":
    case "failed":
    case "timed_out":
      return "error";
    case "cancelled":
    case "canceled":
      return "cancelled";
    case "skipped":
      return "skipped";
    default:
      return undefined;
  }
}

export function toolEventIsCancelled(event: ToolTerminalStatusEvent): boolean {
  return (
    event.cancelled === true ||
    normalizeToolStatus(event.status) === "cancelled" ||
    normalizeReason(event.error_kind) === "cancelled" ||
    normalizeReason(event.reason) === "cancelled"
  );
}

export function toolTerminalStatus(
  event: ToolTerminalStatusEvent,
): ToolStatus {
  const rawStatus = normalizeToolStatus(event.status);

  if (rawStatus === "cancelled" || toolEventIsCancelled(event)) {
    return "cancelled";
  }
  if (rawStatus === "skipped") {
    return "skipped";
  }

  const hasFailure =
    event.success === false ||
    nonEmptyString(event.error_kind) ||
    event.type === "tool_transport_failed";
  if (hasFailure) {
    return "error";
  }

  if (rawStatus === "error") {
    return "error";
  }
  if (rawStatus === "done") {
    return "done";
  }

  return event.skipped === true ? "skipped" : "done";
}

export function planStepResultStatus(result: unknown): "done" | "error" {
  const normalized = normalizeReason(result);
  return normalized === "error" ||
    normalized === "failed" ||
    normalized === "timed_out"
    ? "error"
    : "done";
}

function nonEmptyString(value: unknown): boolean {
  return typeof value === "string" && value.trim().length > 0;
}

function normalizeReason(value: unknown): string | undefined {
  return typeof value === "string" && value.trim().length > 0
    ? value.trim().toLowerCase()
    : undefined;
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
  if (event.type === "run_blocked") {
    return event.reason ?? event.error_kind ?? "blocked";
  }
  if (event.blocked) {
    return event.reason ?? event.error_kind ?? "blocked";
  }
  return null;
}

export type RunWaitingProjection = {
  status: "waiting" | "blocked";
  waitingFor: string;
  blocked: boolean;
};

export function projectRunWaitingState(event: {
  waiting_for?: string | null;
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
