import { extractBlockedReason } from "@/lib/run-status-messages";

type StreamEventLike = Record<string, unknown> & { type?: string };

export function isRunLifecycleEvent(type: string): boolean {
  return type.startsWith("run_");
}

/** A physical stream may carry child observations, but only its root owns lifecycle effects. */
export function isForeignRunLifecycleEvent(
  event: StreamEventLike,
  rootRunId: string | null | undefined,
): boolean {
  return !!rootRunId && typeof event.type === "string" &&
    isRunLifecycleEvent(event.type) && typeof event.run_id === "string" &&
    !!event.run_id.trim() && event.run_id !== rootRunId;
}

export function blockedWaitingFor(event: StreamEventLike): string {
  return (
    extractBlockedReason(
      event as {
        type?: string;
        reason?: string;
        error_kind?: string;
        blocked?: boolean;
      },
    ) ?? "blocked"
  );
}

export function eventMessage(event: StreamEventLike, fallback: string): string {
  for (const key of ["message", "error", "user_message", "reason"] as const) {
    const value = event[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return fallback;
}

export function explicitEventMessage(event: StreamEventLike): string {
  for (const key of ["message", "error", "user_message"] as const) {
    const value = event[key];
    if (typeof value === "string" && value.trim()) {
      return value;
    }
  }
  return "";
}

export function isRunBlockedEvent(type: string): boolean {
  return type === "run_blocked";
}
