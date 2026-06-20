import type { ChatDetail } from "@/lib/api/types";
import { isExecutionBoundaryWait } from "@/lib/run-status-messages";

const QUEUEABLE_RUN_STATUSES = new Set([
  "running",
  "input-queued",
  "waiting",
  "blocked",
]);
const TERMINAL_RUN_STATUSES = new Set(["completed", "failed", "cancelled"]);
const RUN_STATUS_PRIORITY: Record<string, number> = {
  waiting: 4,
  "input-queued": 4,
  cancelling: 3,
  running: 2,
  paused: 1,
};

export function normalizeChatRunStatus(status?: string | null): string | null {
  const normalized = status?.trim().toLowerCase();
  return normalized ? normalized : null;
}

export function isTerminalChatRunStatus(status?: string | null): boolean {
  const normalized = normalizeChatRunStatus(status);
  return Boolean(normalized && TERMINAL_RUN_STATUSES.has(normalized));
}

export function runBlocksChatTurn(status?: string | null): boolean {
  const normalized = normalizeChatRunStatus(status);
  return Boolean(normalized && !TERMINAL_RUN_STATUSES.has(normalized));
}

export function activeRunPriority(run: {
  status: string;
  waitingFor?: string | null;
}): number {
  const normalized = normalizeChatRunStatus(run.status);
  if (!normalized) {
    return 0;
  }
  if (normalized === "paused" && run.waitingFor === "user_input") {
    return RUN_STATUS_PRIORITY["input-queued"];
  }
  return RUN_STATUS_PRIORITY[normalized] ?? 0;
}

function statusLabel(status: string): string {
  return status
    .trim()
    .replace(/^waiting:\s*/i, "")
    .replace(/[_-]+/g, " ")
    .replace(/\s+/g, " ")
    .replace(/\b\w/g, (char) => char.toUpperCase());
}

function activeRunDisplayLabel(
  run: ChatDetail["activeRun"],
  archived: boolean,
  lastQueuedText?: string,
): string {
  const status = normalizeChatRunStatus(run?.status);
  const waitingFor = run?.waitingFor?.trim();
  if (!run?.runId || !status) {
    return archived ? "Archived" : "Active";
  }
  if (status === "cancelling") {
    return "Stopping";
  }
  if (status === "input-queued") {
    const base = "Follow-up Queued";
    if (lastQueuedText) {
      return `${base}: "${compactQueuedText(lastQueuedText)}"`;
    }
    return base;
  }
  if (status === "blocked") {
    return waitingFor ? `Blocked: ${statusLabel(waitingFor)}` : "Blocked";
  }
  if (waitingFor) {
    const label = statusLabel(waitingFor);
    return isExecutionBoundaryWait(waitingFor)
      ? `Blocked: ${label}`
      : `Waiting: ${label}`;
  }
  if (status === "waiting") {
    return "Waiting";
  }
  if (status === "running") {
    return "Running";
  }
  if (status === "paused") {
    return "Paused";
  }
  if (isTerminalChatRunStatus(status)) {
    return statusLabel(status);
  }
  return statusLabel(run.status);
}

export type ChatRunUiState = {
  activeRunStatus: string | null;
  canQueueDeferredInput: boolean;
  canResumeRun: boolean;
  canStopRun: boolean;
  activeRunBlocksNewInput: boolean;
  runControlBusy: boolean;
  composerDisabled: boolean;
  composerPlaceholder: string;
  activeRunLabel: string;
};

export function deriveChatRunUiState(params: {
  activeRun: ChatDetail["activeRun"];
  archived: boolean;
  startingRun: boolean;
  queueingDeferredInput: boolean;
  resumingRun: boolean;
  stoppingRun: boolean;
  lastQueuedText?: string;
}): ChatRunUiState {
  const activeRunStatus = normalizeChatRunStatus(params.activeRun?.status);
  const hasActiveRun = Boolean(params.activeRun?.runId && activeRunStatus);
  const canQueueDeferredInput = Boolean(
    hasActiveRun &&
    activeRunStatus &&
    QUEUEABLE_RUN_STATUSES.has(activeRunStatus),
  );
  const canResumeRun = Boolean(hasActiveRun && activeRunStatus === "paused");
  const canStopRun = Boolean(hasActiveRun && activeRunStatus !== "cancelling");
  const activeRunBlocksNewInput = Boolean(
    hasActiveRun &&
    activeRunStatus &&
    !isTerminalChatRunStatus(activeRunStatus) &&
    !canQueueDeferredInput &&
    !canResumeRun &&
    activeRunStatus !== "cancelling",
  );
  const runControlBusy =
    params.queueingDeferredInput || params.resumingRun || params.stoppingRun;
  const composerDisabled =
    params.startingRun ||
    runControlBusy ||
    activeRunStatus === "paused" ||
    activeRunStatus === "cancelling" ||
    activeRunBlocksNewInput;
  const composerPlaceholder =
    activeRunStatus === "paused"
      ? "Run paused. Resume or stop it to continue."
      : activeRunStatus === "cancelling"
        ? "Stopping current run..."
        : activeRunBlocksNewInput
          ? `Run status is ${params.activeRun?.status ?? "unknown"}. Stop it or refresh before sending.`
          : canQueueDeferredInput
            ? "Queue a follow-up for the next execution boundary..."
            : "Reply to Astra...";
  const activeRunLabel = activeRunDisplayLabel(
    params.activeRun,
    params.archived,
    params.lastQueuedText,
  );

  return {
    activeRunStatus,
    canQueueDeferredInput,
    canResumeRun,
    canStopRun,
    activeRunBlocksNewInput,
    runControlBusy,
    composerDisabled,
    composerPlaceholder,
    activeRunLabel,
  };
}

/// Preview a user's last queued text to show in the run status label.
/// Truncates at ~60 characters to fit in the activity bar.
export function compactQueuedText(text: string): string {
  const singleLine = text.trim().replace(/\s+/g, " ");
  const chars = Array.from(singleLine);
  if (chars.length <= 60) {
    return singleLine;
  }
  return `${chars.slice(0, 57).join("")}…`;
}
