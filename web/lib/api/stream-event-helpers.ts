import { extractBlockedReason } from "@/lib/run-status-messages";

type StreamEventLike = Record<string, unknown> & { type?: string };

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
