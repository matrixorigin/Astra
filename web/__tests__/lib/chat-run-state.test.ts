import { deriveChatRunUiState } from '@/lib/chat-run-state';
import type { ChatDetail } from '@/lib/api/types';

const base = {
  archived: false,
  startingRun: false,
  queueingDeferredInput: false,
  resumingRun: false,
  stoppingRun: false,
};

function run(status: string): NonNullable<ChatDetail['activeRun']> {
  return {
    runId: `run-${status}`,
    status,
    waitingFor: null,
  };
}

function waitingRun(
  status: string,
  waitingFor: string,
): NonNullable<ChatDetail['activeRun']> {
  return {
    runId: `run-${status}`,
    status,
    waitingFor,
  };
}

describe('deriveChatRunUiState', () => {
  it('allows queued follow-up input for active queueable statuses', () => {
    for (const status of ['running', 'input-queued', 'waiting', 'blocked']) {
      const ui = deriveChatRunUiState({ ...base, activeRun: run(status) });

      expect(ui.canQueueDeferredInput).toBe(true);
      expect(ui.composerDisabled).toBe(false);
      expect(ui.composerPlaceholder).toBe('Message Astra while it works...');
    }
  });

  it('keeps terminal active-run statuses from triggering deferred queue mode', () => {
    for (const status of ['completed', 'failed', 'cancelled']) {
      const ui = deriveChatRunUiState({ ...base, activeRun: run(status) });

      expect(ui.canQueueDeferredInput).toBe(false);
      expect(ui.activeRunBlocksNewInput).toBe(false);
      expect(ui.composerDisabled).toBe(false);
      expect(ui.composerPlaceholder).toBe('Reply to Astra...');
    }
  });

  it('blocks unknown non-terminal statuses instead of silently enabling queue mode', () => {
    const ui = deriveChatRunUiState({ ...base, activeRun: run('initializing-provider') });

    expect(ui.canQueueDeferredInput).toBe(false);
    expect(ui.activeRunBlocksNewInput).toBe(true);
    expect(ui.composerDisabled).toBe(true);
    expect(ui.composerPlaceholder).toBe(
      'Astra is busy. Stop it or wait to continue.',
    );
    expect(ui.activeRunLabel).toBe('Initializing Provider');
  });

  it('labels execution-boundary waits as blocked instead of leaking raw reasons', () => {
    const ui = deriveChatRunUiState({
      ...base,
      activeRun: waitingRun('blocked', 'executor_offline'),
    });

    expect(ui.activeRunLabel).toBe('Environment Offline');
  });

  it('labels ordinary waits separately from blocked executor states', () => {
    const ui = deriveChatRunUiState({
      ...base,
      activeRun: waitingRun('waiting', 'tool_approval'),
    });

    expect(ui.activeRunLabel).toBe('Waiting for Approval');
  });

  it('disables composer consistently while any run-control mutation is in flight', () => {
    for (const busy of [
      { queueingDeferredInput: true },
      { resumingRun: true },
      { stoppingRun: true },
    ]) {
      const ui = deriveChatRunUiState({
        ...base,
        ...busy,
        activeRun: run('running'),
      });

      expect(ui.runControlBusy).toBe(true);
      expect(ui.composerDisabled).toBe(true);
      expect(ui.canStopRun).toBe(true);
    }
  });

  it('requires explicit resume or stop for paused runs', () => {
    const ui = deriveChatRunUiState({ ...base, activeRun: run('paused') });

    expect(ui.canQueueDeferredInput).toBe(false);
    expect(ui.canResumeRun).toBe(true);
    expect(ui.canStopRun).toBe(true);
    expect(ui.composerDisabled).toBe(true);
    expect(ui.composerPlaceholder).toBe('Paused. Resume or stop to continue.');
  });

  it('labels archived idle chats without disabling the composer decision itself', () => {
    const ui = deriveChatRunUiState({
      ...base,
      archived: true,
      activeRun: undefined,
    });

    expect(ui.activeRunLabel).toBe('Archived');
    expect(ui.composerDisabled).toBe(false);
  });
});
