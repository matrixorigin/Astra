import type { ChatDetail } from '@/lib/api/types';

const QUEUEABLE_RUN_STATUSES = new Set(['running', 'input-queued', 'waiting']);
const TERMINAL_RUN_STATUSES = new Set(['completed', 'failed', 'cancelled']);

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
  activeRun: ChatDetail['activeRun'];
  archived: boolean;
  startingRun: boolean;
  queueingDeferredInput: boolean;
  resumingRun: boolean;
  stoppingRun: boolean;
}): ChatRunUiState {
  const activeRunStatus = params.activeRun?.status.trim().toLowerCase() ?? null;
  const hasActiveRun = Boolean(params.activeRun?.runId && activeRunStatus);
  const canQueueDeferredInput = Boolean(
    hasActiveRun && activeRunStatus && QUEUEABLE_RUN_STATUSES.has(activeRunStatus),
  );
  const canResumeRun = Boolean(hasActiveRun && activeRunStatus === 'paused');
  const canStopRun = Boolean(hasActiveRun && activeRunStatus !== 'cancelling');
  const activeRunBlocksNewInput = Boolean(
    hasActiveRun
      && activeRunStatus
      && !TERMINAL_RUN_STATUSES.has(activeRunStatus)
      && !canQueueDeferredInput
      && !canResumeRun
      && activeRunStatus !== 'cancelling',
  );
  const runControlBusy = params.queueingDeferredInput || params.resumingRun || params.stoppingRun;
  const composerDisabled = params.startingRun
    || runControlBusy
    || activeRunStatus === 'paused'
    || activeRunStatus === 'cancelling'
    || activeRunBlocksNewInput;
  const composerPlaceholder = activeRunStatus === 'paused'
    ? 'Run paused. Resume or stop it to continue.'
    : activeRunStatus === 'cancelling'
      ? 'Stopping current run...'
      : activeRunBlocksNewInput
        ? `Run status is ${params.activeRun?.status ?? 'unknown'}. Stop it or refresh before sending.`
        : canQueueDeferredInput
          ? 'Queue a follow-up for the next tool call...'
          : 'Reply to Astra...';
  const activeRunLabel = activeRunStatus === 'cancelling'
    ? 'Stopping current run'
    : activeRunStatus === 'input-queued'
      ? 'Input queued for next tool call'
      : params.activeRun?.waitingFor
        ? `Waiting for ${params.activeRun.waitingFor}`
        : params.activeRun?.runId
          ? `Run ${params.activeRun.status}`
          : params.archived
            ? 'Archived'
            : 'Active';

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
