import type {
  AgentCancelledEvent,
  AgentFailedEvent,
  AgentInterruptedEvent,
  RunErrorEvent,
  RunInterruptedEvent,
  ToolCallEvent,
  WorkspaceState,
} from '../index';
import type {
  ExecutorBinding,
  WorkspaceBinding,
} from '../react';

describe('public SDK types', () => {
  test('export execution-boundary run, tool, agent, and workspace state types', () => {
    const workspace: WorkspaceBinding = {
      kind: 'edge_workspace',
      cwd: '/repo',
      fallback_policy: 'disabled',
    };
    const executor: ExecutorBinding = {
      kind: 'edge_agent',
      executor_id: 'edge-1',
      status: 'online',
    };
    const state: WorkspaceState = {
      sessionId: 'session-1',
      runId: 'run-1',
      runStatus: 'blocked',
      waitingFor: 'executor_offline',
      workspace,
      executor,
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
      messages: [],
      toolCalls: [],
      followupSuggestion: null,
      isStreaming: false,
      error: null,
      plan: null,
      usage: {
        promptTokens: 0,
        completionTokens: 0,
        totalTokens: 0,
        cacheCreationTokens: 0,
        cacheReadTokens: 0,
      },
      agentEvents: [],
    };
    const toolCall: ToolCallEvent = {
      type: 'tool_call',
      tool_call: { id: 'call-1', function: { name: 'bash' } },
      workspace,
      executor,
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    };
    const runError: RunErrorEvent = {
      type: 'run_error',
      run_id: 'run-1',
      message: 'failed',
      workspace,
      executor,
    };
    const interrupted: RunInterruptedEvent = {
      type: 'run_interrupted',
      run_id: 'run-1',
      waiting_for: 'user_resume',
      resumable: true,
    };
    const failedAgent: AgentFailedEvent = {
      type: 'agent_failed',
      agent_id: 'agent-1',
      error: 'failed',
      workspace,
    };
    const cancelledAgent: AgentCancelledEvent = {
      type: 'agent_cancelled',
      agent_id: 'agent-2',
      reason: 'parent stopped',
      executor,
    };
    const interruptedAgent: AgentInterruptedEvent = {
      type: 'agent_interrupted',
      agent_id: 'agent-3',
      reason: 'stop requested',
    };

    expect([
      state.workspace?.kind,
      toolCall.workspace?.kind,
      runError.executor?.kind,
      interrupted.waiting_for,
      failedAgent.workspace?.kind,
      cancelledAgent.executor?.kind,
      interruptedAgent.type,
    ]).toEqual([
      'edge_workspace',
      'edge_workspace',
      'edge_agent',
      'user_resume',
      'edge_workspace',
      'edge_agent',
      'agent_interrupted',
    ]);
  });
});
