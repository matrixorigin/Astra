import type { StreamEventState } from "@/lib/api/stream-event-handler";
import { extractBlockedReason } from "@/lib/run-status-messages";
import { WebApiError } from "@/lib/api/errors";

type StreamEventLike = Record<string, unknown> & { type?: string };

const RUN_FINISHED_STATUSES = ["completed", "failed", "cancelled", "paused", "interrupted", "delegated"];

export function rootRunFinishedStatus(event: StreamEventLike, runId: string | undefined): string {
  if (!runId || event.run_id !== runId || typeof event.status !== "string" ||
    !RUN_FINISHED_STATUSES.includes(event.status)) {
    throw new WebApiError(502, "Invalid root run completion.", "contract_violation");
  }
  return event.status;
}

export function isResumableRunLifecycle(lifecycle: StreamEventState["runLifecycle"]) {
  return lifecycle === "paused" || lifecycle === "waiting" || lifecycle === "blocked";
}

/** Both a confirmed resume response and its stream receipt resume this same projection. */
export function resumeStreamProjection(state: StreamEventState) {
  state.runLifecycle = "running";
  state.lastStatus = "streaming";
  state.terminalStatus = undefined;
}

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

/** Idempotent across both physical connections and authoritative text replacement. */
export function appendRunFailure(text: string, reason: string): string {
  return !text ? reason : text === reason || text.endsWith(`\n\n${reason}`) ? text : `${text}\n\n${reason}`;
}

export type ChatStreamInitialization = {
  type: "chat_stream_init";
  assistant_message_id: string;
  session_id: string;
  state: Pick<StreamEventState, "assistantRawText" | "assistantText" | "reasoningText" |
    "reasoningFromThinkingTags" | "lastStatus" | "runLifecycle" | "runId" |
    "nextEventIndex" | "lastFailureMessage" | "errorCode" | "terminalStatus">;
};

export function isChatStreamInitialization(event: Record<string, unknown>): event is ChatStreamInitialization {
  const s = event.state;
  if (event.type !== "chat_stream_init" || typeof event.assistant_message_id !== "string" ||
    !event.assistant_message_id.trim() || typeof event.session_id !== "string" || !event.session_id.trim() ||
    !s || typeof s !== "object" || Array.isArray(s)) return false;
  const state = s as Record<string, unknown>;
  return ["assistantRawText", "assistantText", "reasoningText"].every(key => typeof state[key] === "string") &&
    ["runId", "lastFailureMessage", "errorCode"].every(key => state[key] === undefined || typeof state[key] === "string") &&
    (state.reasoningFromThinkingTags === undefined || typeof state.reasoningFromThinkingTags === "boolean") &&
    typeof state.lastStatus === "string" && ["streaming", "complete", "failed"].includes(state.lastStatus) &&
    typeof state.runLifecycle === "string" && ["running", "paused", "waiting", "blocked", "finished"].includes(state.runLifecycle) &&
    (state.terminalStatus === undefined || typeof state.terminalStatus === "string" &&
      RUN_FINISHED_STATUSES.includes(state.terminalStatus)) &&
    (state.nextEventIndex === undefined || typeof state.nextEventIndex === "number" &&
      Number.isSafeInteger(state.nextEventIndex) && state.nextEventIndex >= 0);
}
