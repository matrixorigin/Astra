import {
  applyWorkSurfaceEvent,
  createEmptyWorkSurface,
  hydrateWorkSurface,
  resetWorkSurfaceForRun,
} from '@/lib/work-surface';

const task = {
  id: 'task-1',
  title: 'Implement panel',
  status: 'in_progress',
  created_at: '2026-06-10T00:00:00.000Z',
  updated_at: '2026-06-10T00:00:00.000Z',
};

describe('work surface reducer', () => {
  it('applies task board snapshots as the authoritative task state', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: 'task_board_snapshot',
      session_id: 'session-1',
      run_id: 'run-1',
      reason: 'task_update',
      workspace: {
        kind: 'server_sandbox',
        display_name: 'Server sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
      tasks: [task],
    });

    expect(state.sessionId).toBe('session-1');
    expect(state.runId).toBe('run-1');
    expect(state.workspace).toMatchObject({
      kind: 'server_sandbox',
      cwd: '/tmp/astra-workspaces/session-1',
    });
    expect(state.executor).toMatchObject({
      kind: 'server_local',
      transport: 'server_local',
    });
    expect(state.hydrated).toBe(true);
    expect(state.loading).toBe(false);
    expect(state.tasks).toEqual([task]);
  });

  it('does not carry stale bindings across hydrated projections', () => {
    let state = applyWorkSurfaceEvent(
      createEmptyWorkSurface('session-1', 'run-old'),
      {
        type: 'workspace_bound',
        workspace: {
          kind: 'edge_workspace',
          display_name: 'MacBook Pro',
          cwd: '/Users/xupeng/github/astra',
          authority: 'read_write',
          fallback_policy: 'disabled',
        },
        executor: {
          kind: 'edge_agent',
          executor_id: 'edge-macbook-1',
          display_name: 'MacBook Pro',
          transport: 'edge_ws',
          status: 'online',
        },
      },
    );

    state = hydrateWorkSurface(state, {
      sessionId: 'session-1',
      runId: 'run-new',
      tasks: [],
      events: [],
    });

    expect(state.workspace).toBeUndefined();
    expect(state.executor).toBeUndefined();
  });

  it('hydrates top-level projection bindings even when recent events omit binding events', () => {
    const state = hydrateWorkSurface(
      createEmptyWorkSurface('session-1', 'run-1'),
      {
        sessionId: 'session-1',
        runId: 'run-1',
        status: 'running',
        workspace: {
          kind: 'edge_workspace',
          display_name: 'MacBook Pro',
          cwd: '/Users/xupeng/github/astra',
          authority: 'read_write',
          fallback_policy: 'disabled',
        },
        executor: {
          kind: 'edge_agent',
          executor_id: 'edge-macbook-1',
          display_name: 'MacBook Pro',
          transport: 'edge_ws',
          status: 'online',
        },
        transport: 'edge_ws',
        fallbackPolicy: 'disabled',
        tasks: [],
        events: [{ type: 'text_done', full_text: 'done' }],
      },
    );

    expect(state.workspace).toMatchObject({
      kind: 'edge_workspace',
      cwd: '/Users/xupeng/github/astra',
    });
    expect(state.executor).toMatchObject({
      kind: 'edge_agent',
      executor_id: 'edge-macbook-1',
    });
  });

  it('rebuilds bindings from hydrated projection events', () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface('session-1'), {
      sessionId: 'session-1',
      runId: 'run-new',
      tasks: [],
      events: [
        {
          type: 'workspace_bound',
          workspace: {
            kind: 'server_sandbox',
            display_name: 'Server sandbox',
            cwd: '/tmp/astra-workspaces/session-1',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'server_local',
            executor_id: 'server-local',
            display_name: 'Server sandbox',
            transport: 'server_local',
            status: 'online',
          },
        },
      ],
    });

    expect(state.workspace).toMatchObject({
      kind: 'server_sandbox',
      cwd: '/tmp/astra-workspaces/session-1',
    });
    expect(state.executor).toMatchObject({
      kind: 'server_local',
      transport: 'server_local',
    });
  });

  it('rebuilds bindings from run_started projection events', () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface('session-1'), {
      sessionId: 'session-1',
      runId: 'run-new',
      tasks: [],
      events: [
        {
          type: 'run_started',
          run_id: 'run-new',
          session_id: 'session-1',
          workspace: {
            kind: 'edge_workspace',
            display_name: 'MacBook Pro',
            cwd: '/Users/xupeng/github/astra',
            authority: 'read_write',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-macbook-1',
            display_name: 'MacBook Pro',
            transport: 'edge_ws',
            status: 'online',
          },
          transport: 'edge_ws',
          fallback_policy: 'disabled',
        },
      ],
    });

    expect(state.runId).toBe('run-new');
    expect(state.workspace).toMatchObject({
      kind: 'edge_workspace',
      cwd: '/Users/xupeng/github/astra',
    });
    expect(state.executor).toMatchObject({
      kind: 'edge_agent',
      executor_id: 'edge-macbook-1',
      transport: 'edge_ws',
    });
  });

  it('tracks current-protocol tool calls through completion', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_call',
      tool_call: {
        id: 'call-1',
        function: {
          name: 'bash',
          arguments: { command: 'echo hi' },
        },
      },
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_call_end',
      call_id: 'call-1',
      result: 'Error: denied',
      success: false,
    });

    expect(state.tools).toHaveLength(1);
    expect(state.tools[0]).toMatchObject({
      callId: 'call-1',
      tool: 'bash',
      arguments: '{"command":"echo hi"}',
      result: 'Error: denied',
      status: 'error',
    });
  });

  it('keeps skipped tool calls distinct from success and failure', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_call_start',
      call_id: 'call-skip',
      tool: 'read_file',
      arguments: { path: 'README.md' },
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_call_end',
      call_id: 'call-skip',
      tool: 'read_file',
      status: 'skipped',
      skipped: true,
      success: true,
      result: 'Duplicate call skipped.',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-skip',
      tool: 'read_file',
      status: 'skipped',
      result: 'Duplicate call skipped.',
    });
  });

  it('projects workspace and executor bindings onto live tool cards', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'workspace_bound',
      session_id: 'session-1',
      workspace: {
        kind: 'server_sandbox',
        display_name: 'Server sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
      fallback_policy: 'disabled',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_routing_decision',
      call_id: 'call-1',
      tool: 'bash',
      route: 'server_local',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_call_end',
      call_id: 'call-1',
      tool: 'bash',
      result: 'ok',
      success: true,
      duration_ms: 12,
    });

    expect(state.workspace?.kind).toBe('server_sandbox');
    expect(state.executor?.kind).toBe('server_local');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-1',
      tool: 'bash',
      status: 'done',
      workspace: {
        kind: 'server_sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
      },
      executor: {
        kind: 'server_local',
        transport: 'server_local',
      },
      transport: 'server_local',
      fallbackPolicy: 'disabled',
      route: 'server_local',
      durationMs: 12,
    });
  });

  it('keeps run workspace while projecting server-runtime tool metadata', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'workspace_bound',
      session_id: 'session-1',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/test/project',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_routing_decision',
      call_id: 'call-web',
      tool: 'web_search',
      route: 'server_runtime',
      workspace: {
        kind: 'none',
        display_name: 'No workspace',
        authority: 'none',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-runtime',
        display_name: 'Server runtime',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
      fallback_policy: 'disabled',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-web',
      tool: 'web_search',
      status: 'running',
      workspace: {
        kind: 'none',
        display_name: 'No workspace',
      },
      executor: {
        kind: 'server_local',
        display_name: 'Server runtime',
      },
      transport: 'server_local',
      route: 'server_runtime',
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_completed',
      call_id: 'call-web',
      tool: 'web_search',
      success: true,
      duration_ms: 8,
      workspace: {
        kind: 'none',
        display_name: 'No workspace',
        authority: 'none',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-runtime',
        display_name: 'Server runtime',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
      fallback_policy: 'disabled',
    });

    expect(state.workspace).toMatchObject({
      kind: 'edge_workspace',
      cwd: '/Users/test/project',
    });
    expect(state.executor).toMatchObject({
      kind: 'edge_agent',
      transport: 'edge_ws',
    });
    expect(state.tools[0]).toMatchObject({
      callId: 'call-web',
      tool: 'web_search',
      status: 'done',
      workspace: {
        kind: 'none',
        display_name: 'No workspace',
      },
      executor: {
        kind: 'server_local',
        display_name: 'Server runtime',
        transport: 'server_local',
      },
      transport: 'server_local',
      route: 'server_runtime',
      durationMs: 8,
    });
  });

  it('finalizes running tool and agent cards when a run is cancelled', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_started',
      run_id: 'run-1',
      session_id: 'session-1',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_started',
      call_id: 'call-running',
      tool: 'bash',
      arguments: { command: 'sleep 60' },
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_spawned',
      agent_id: 'agent-running',
      run_id: 'child-run',
      parent_run_id: 'run-1',
      agent_type: 'code-review',
      description: 'Review while parent is running',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-running',
      status: 'llm_call_started',
      turn: 1,
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'run_finished',
      run_id: 'run-1',
      session_id: 'session-1',
      status: 'cancelled',
      timestamp: 1_801_000_000_000,
    });

    expect(state.runStatus).toBe('cancelled');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-running',
      status: 'cancelled',
      errorKind: 'cancelled',
      result: 'Stopped before this tool emitted a final transport result.',
      finishedAt: 1_801_000_000_000,
    });
    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-running',
      status: 'cancelled',
      reason: 'parent_run_cancelled',
      resultSummary: 'Stopped with the parent run.',
      updatedAt: 1_801_000_000_000,
    });
    expect(state.blocked).toBeNull();
  });

  it('finalizes running tool and agent cards when a run fails', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_started',
      run_id: 'run-1',
      session_id: 'session-1',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_started',
      call_id: 'call-running',
      tool: 'bash',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_spawned',
      agent_id: 'agent-running',
      run_id: 'child-run',
      parent_run_id: 'run-1',
      agent_type: 'code-review',
      description: 'Review while parent is running',
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'run_error',
      run_id: 'run-1',
      message: 'loop crashed',
      error_kind: 'runtime',
      timestamp: 1_801_000_000_000,
    });

    expect(state.runStatus).toBe('failed');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-running',
      status: 'error',
      errorKind: 'runtime',
      result: 'loop crashed',
      finishedAt: 1_801_000_000_000,
    });
    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-running',
      status: 'failed',
      error: 'loop crashed',
      reason: 'runtime',
      updatedAt: 1_801_000_000_000,
    });
  });

  it('marks active tool and agent cards interrupted when a run pauses from interruption', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_started',
      run_id: 'run-1',
      session_id: 'session-1',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_started',
      call_id: 'call-running',
      tool: 'bash',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_spawned',
      agent_id: 'agent-running',
      run_id: 'child-run',
      parent_run_id: 'run-1',
      agent_type: 'code-review',
      description: 'Review while parent is running',
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'run_interrupted',
      run_id: 'run-1',
      kind: 'budget_exhausted',
      resumable: true,
      message: 'Budget exhausted. You can continue.',
      timestamp: 1_801_000_000_000,
    });

    expect(state.runStatus).toBe('paused');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-running',
      status: 'error',
      errorKind: 'budget_exhausted',
      result: 'Run paused before this tool emitted a final transport result.',
      finishedAt: 1_801_000_000_000,
    });
    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-running',
      status: 'interrupted',
      reason: 'budget_exhausted',
      resultSummary: 'Budget exhausted. You can continue.',
      updatedAt: 1_801_000_000_000,
    });
  });

  it('treats hydrated terminal run status as authoritative when run_finished is outside the event window', () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface('session-1'), {
      sessionId: 'session-1',
      runId: 'run-1',
      status: 'cancelled',
      tasks: [],
      events: [
        {
          type: 'tool_transport_started',
          call_id: 'call-stale',
          tool: 'bash',
          arguments: { command: 'sleep 60' },
        },
        {
          type: 'agent_spawned',
          agent_id: 'agent-stale',
          run_id: 'child-run',
          parent_run_id: 'run-1',
          agent_type: 'code-review',
          description: 'Review branch',
        },
      ],
    });

    expect(state.runStatus).toBe('cancelled');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-stale',
      status: 'cancelled',
      errorKind: 'cancelled',
    });
    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-stale',
      status: 'cancelled',
      reason: 'parent_run_cancelled',
    });
  });

  it('marks transport completion as a finished tool card before tool_call_end arrives', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'workspace_bound',
      workspace: {
        kind: 'server_sandbox',
        display_name: 'Server sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_started',
      call_id: 'call-1',
      tool: 'bash',
      arguments: { command: 'printf ok' },
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'tool_transport_completed',
      call_id: 'call-1',
      tool: 'bash',
      duration_ms: 18,
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-1',
      tool: 'bash',
      status: 'done',
      durationMs: 18,
      transport: 'server_local',
      fallbackPolicy: 'disabled',
    });
    expect(state.tools[0]?.finishedAt).toBeDefined();
  });

  it('projects executor-offline blocks as actionable run state', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-offline',
      tool: 'bash',
      success: false,
      error:
        "Error: executor 'MacBook Pro' is offline. Server fallback is disabled.",
      error_kind: 'executor_offline',
      blocked: true,
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'run_blocked',
      call_id: 'call-offline',
      tool: 'bash',
      reason: 'executor_offline',
      message:
        'Edge executor MacBook Pro is offline. Reconnect edge or choose a new executor.',
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-offline',
      status: 'error',
      errorKind: 'executor_offline',
      blocked: true,
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
    });
    expect(state.runStatus).toBe('blocked');
    expect(state.blocked).toMatchObject({
      reason: 'executor_offline',
      message:
        'Edge executor MacBook Pro is offline. Reconnect edge or choose a new executor.',
      callId: 'call-offline',
      tool: 'bash',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'executor_status_changed',
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
    });
    expect(state.blocked).toBeNull();
    expect(state.runStatus).toBe('running');
  });

  it('projects non-offline blocked transport failures without losing cause', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-disconnect',
      tool: 'bash',
      success: false,
      error: 'Edge WebSocket disconnected before the tool result arrived.',
      error_kind: 'transport_disconnected',
      blocked: true,
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'degraded',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-disconnect',
      status: 'error',
      errorKind: 'transport_disconnected',
      blocked: true,
    });
    expect(state.blocked).toMatchObject({
      reason: 'transport_disconnected',
      message: 'Edge WebSocket disconnected before the tool result arrived.',
      callId: 'call-disconnect',
      tool: 'bash',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        status: 'degraded',
      },
      fallbackPolicy: 'disabled',
    });
    expect(state.runStatus).toBe('blocked');

    state = applyWorkSurfaceEvent(state, {
      type: 'executor_status_changed',
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
    });
    expect(state.blocked).toBeNull();
    expect(state.runStatus).toBe('running');
  });

  it('projects explicit transport-disconnected run blocked events', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_blocked',
      call_id: 'call-disconnect',
      tool: 'bash',
      reason: 'transport_disconnected',
      message: 'Edge transport disconnected before the tool result arrived.',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'degraded',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.blocked).toMatchObject({
      reason: 'transport_disconnected',
      message: 'Edge transport disconnected before the tool result arrived.',
      callId: 'call-disconnect',
      tool: 'bash',
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
      executor: {
        kind: 'edge_agent',
        status: 'degraded',
      },
    });
    expect(state.runStatus).toBe('blocked');
  });

  it('treats execution-boundary run_waiting events as blocked', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_waiting',
      run_id: 'run-1',
      reason: 'waiting: executor_offline',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.runStatus).toBe('blocked');
    expect(state.blocked).toMatchObject({
      reason: 'executor_offline',
      message:
        'Executor is offline. Reconnect the selected edge executor or choose another workspace.',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });
  });

  it('projects generic run_waiting events without stale blocked state', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_blocked',
      reason: 'fallback_disabled',
      message: 'Server fallback is disabled for this workspace.',
    });
    expect(state.blocked).not.toBeNull();

    state = applyWorkSurfaceEvent(state, {
      type: 'run_waiting',
      run_id: 'run-1',
      reason: 'waiting: tool_approval',
    });

    expect(state.runStatus).toBe('waiting');
    expect(state.blocked).toBeNull();
  });

  it('retains active tools when long timelines exceed the display cap', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_call_start',
      call_id: 'call-long-running',
      tool: 'bash',
      timestamp: 1_800_000_000_000,
    });

    for (let i = 0; i < 40; i += 1) {
      state = applyWorkSurfaceEvent(state, {
        type: 'tool_call_end',
        call_id: `call-done-${i}`,
        tool: 'read_file',
        success: true,
        result: `done-${i}`,
        timestamp: 1_800_000_001_000 + i,
      });
    }

    expect(state.tools).toHaveLength(40);
    expect(
      state.tools.some((tool) => tool.callId === 'call-long-running'),
    ).toBe(true);
    expect(
      state.tools.find((tool) => tool.callId === 'call-long-running')?.status,
    ).toBe('running');
  });

  it('retains active agents when fanout history exceeds the display cap', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'agent_spawned',
      agent_id: 'agent-long-running',
      agent_type: 'code-review',
      description: 'long review',
      timestamp: 1_800_000_000_000,
    });

    for (let i = 0; i < 60; i += 1) {
      state = applyWorkSurfaceEvent(state, {
        type: 'agent_completed',
        agent_id: `agent-done-${i}`,
        agent_type: 'explore',
        description: `done ${i}`,
        status: 'completed',
        timestamp: 1_800_000_001_000 + i,
      });
    }

    expect(state.agents).toHaveLength(60);
    expect(
      state.agents.some((agent) => agent.agentId === 'agent-long-running'),
    ).toBe(true);
    expect(
      state.agents.find((agent) => agent.agentId === 'agent-long-running')
        ?.status,
    ).toBe('running');
  });

  it('projects run_blocked reason fields without a reducer release', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'run_blocked',
      call_id: 'call-write',
      tool: 'write_file',
      reason: 'fallback_disabled',
      message: 'Server fallback is disabled for this workspace.',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.blocked).toMatchObject({
      reason: 'fallback_disabled',
      message: 'Server fallback is disabled for this workspace.',
      callId: 'call-write',
      tool: 'write_file',
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
    });
    expect(state.runStatus).toBe('blocked');
  });

  it('projects unavailable workspace executor as an execution-boundary block', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-cloud',
      tool: 'bash',
      success: false,
      error:
        "Error: workspace 'Cloud checkout' (git_checkout) is not routed to an available executor transport. No server fallback was attempted.",
      error_kind: 'workspace_executor_unavailable',
      reason: 'workspace_executor_unavailable',
      blocked: true,
      workspace: {
        kind: 'git_checkout',
        display_name: 'Cloud checkout',
        cwd: '/checkout/repo',
        authority: 'read_only',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'orchestrator_managed',
        executor_id: 'orchestrator-managed',
        display_name: 'Orchestrator-managed executor',
        transport: 'sandbox_resident_agent',
        status: 'degraded',
      },
      transport: 'sandbox_resident_agent',
      fallback_policy: 'disabled',
    });

    expect(state.runStatus).toBe('blocked');
    expect(state.tools[0]).toMatchObject({
      callId: 'call-cloud',
      tool: 'bash',
      status: 'error',
      errorKind: 'workspace_executor_unavailable',
      blocked: true,
      workspace: { kind: 'git_checkout', cwd: '/checkout/repo' },
      executor: { kind: 'orchestrator_managed', status: 'degraded' },
      transport: 'sandbox_resident_agent',
      fallbackPolicy: 'disabled',
    });
    expect(state.blocked).toMatchObject({
      reason: 'workspace_executor_unavailable',
      tool: 'bash',
      callId: 'call-cloud',
      workspace: { kind: 'git_checkout', cwd: '/checkout/repo' },
      executor: { kind: 'orchestrator_managed', status: 'degraded' },
      transport: 'sandbox_resident_agent',
      fallbackPolicy: 'disabled',
    });
  });

  it('keeps approval-timeout blocked state until the approval flow changes', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_call_end',
      call_id: 'call-approval',
      tool: 'write_file',
      success: false,
      result: 'Approval timed out.',
      error_kind: 'approval_timeout',
      blocked: true,
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
    });
    expect(state.blocked).toMatchObject({
      reason: 'approval_timeout',
      message: 'Approval timed out.',
      tool: 'write_file',
    });
    expect(state.runStatus).toBe('blocked');

    state = applyWorkSurfaceEvent(state, {
      type: 'executor_status_changed',
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
    });
    expect(state.blocked).toMatchObject({
      reason: 'approval_timeout',
      message: 'Approval timed out.',
    });
    expect(state.runStatus).toBe('blocked');
  });

  it('projects tool-timeout failures without blocking the whole run', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-timeout',
      tool: 'bash',
      success: false,
      error: 'Error: [bash timed out after 0.1s; partial output shown]',
      error_kind: 'tool_timeout',
      reason: 'tool_timeout',
      workspace: {
        kind: 'server_sandbox',
        display_name: 'Server sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-timeout',
      tool: 'bash',
      status: 'error',
      errorKind: 'tool_timeout',
      blocked: undefined,
      workspace: {
        kind: 'server_sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
      },
      executor: {
        kind: 'server_local',
        status: 'online',
      },
    });
    expect(state.blocked).toBeNull();
  });

  it('projects cancelled tool transport as cancellation instead of failure', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-cancelled',
      tool: 'bash',
      success: false,
      error: "Tool 'bash' cancelled before completion",
      error_kind: 'cancelled',
      reason: 'cancelled',
      cancelled: true,
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.runStatus).toBeNull();
    expect(state.blocked).toBeNull();
    expect(state.tools[0]).toMatchObject({
      callId: 'call-cancelled',
      tool: 'bash',
      status: 'cancelled',
      errorKind: 'cancelled',
      blocked: undefined,
      result: "Tool 'bash' cancelled before completion",
      workspace: { kind: 'edge_workspace' },
      executor: { kind: 'edge_agent' },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });
  });

  it('does not project cancelled transport hints as blocked run state', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-cancelled-blocked',
      tool: 'bash',
      success: false,
      error: "Tool 'bash' cancelled before completion",
      error_kind: 'cancelled',
      reason: 'cancelled',
      cancelled: true,
      blocked: true,
    });

    expect(state.runStatus).toBeNull();
    expect(state.blocked).toBeNull();
    expect(state.tools[0]).toMatchObject({
      callId: 'call-cancelled-blocked',
      status: 'cancelled',
      errorKind: 'cancelled',
      blocked: true,
    });
  });

  it('projects workspace path mismatches as actionable blocked state', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-path',
      tool: 'bash',
      success: false,
      error:
        "Error: command references local path '~/github/astra', but this run is bound to Server sandbox.",
      error_kind: 'workspace_path_mismatch',
      reason: 'workspace_path_mismatch',
      blocked: true,
      workspace: {
        kind: 'server_sandbox',
        display_name: 'Server sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'server_local',
        executor_id: 'server-local',
        display_name: 'Server sandbox',
        transport: 'server_local',
        status: 'online',
      },
      transport: 'server_local',
    });

    expect(state.tools[0]).toMatchObject({
      callId: 'call-path',
      tool: 'bash',
      status: 'error',
      errorKind: 'workspace_path_mismatch',
      blocked: true,
      workspace: {
        kind: 'server_sandbox',
        cwd: '/tmp/astra-workspaces/session-1',
      },
      executor: {
        kind: 'server_local',
        transport: 'server_local',
      },
    });
    expect(state.blocked).toMatchObject({
      reason: 'workspace_path_mismatch',
      message:
        "Error: command references local path '~/github/astra', but this run is bound to Server sandbox.",
      callId: 'call-path',
      tool: 'bash',
      transport: 'server_local',
      fallbackPolicy: 'disabled',
    });
    expect(state.runStatus).toBe('blocked');
  });

  it('projects workspace and executor bindings onto live agent cards', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'agent_spawned',
      agent_id: 'agent-1',
      run_id: 'child-run',
      parent_run_id: 'parent-run',
      agent_type: 'code-review',
      description: 'Review the branch',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_completed',
      agent_id: 'agent-1',
      result_summary: 'No blockers',
      total_tool_calls: 2,
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-1',
      runId: 'child-run',
      parentRunId: 'parent-run',
      status: 'completed',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        transport: 'edge_ws',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
      resultSummary: 'No blockers',
    });
  });

  it('projects agent waiting as a visible terminal child state', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'agent_spawned',
      agent_id: 'agent-1',
      run_id: 'child-run',
      parent_run_id: 'parent-run',
      agent_type: 'code-review',
      description: 'Review the branch',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_waiting',
      agent_id: 'agent-1',
      reason: 'executor_offline',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-1',
      status: 'waiting',
      reason: 'executor_offline',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });
    expect(state.agents[0].events?.at(-1)).toMatchObject({
      label: 'Waiting',
      detail: 'executor_offline',
      tone: 'danger',
    });
  });

  it('projects agent waiting from structured agent tool failures', () => {
    const state = applyWorkSurfaceEvent(createEmptyWorkSurface('session-1'), {
      type: 'tool_transport_failed',
      call_id: 'call-agent',
      tool: 'agent',
      success: false,
      error_kind: 'executor_offline',
      reason: 'executor_offline',
      blocked: true,
      agent_id: 'agent-1',
      agent_status: 'waiting',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/xupeng/github/astra',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'offline',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-1',
      status: 'waiting',
      reason: 'executor_offline',
      workspace: {
        kind: 'edge_workspace',
      },
      executor: {
        kind: 'edge_agent',
        status: 'offline',
      },
    });
    expect(state.agents[0].events?.at(-1)).toMatchObject({
      label: 'Waiting',
      detail: 'executor_offline',
    });
  });

  it('clears run-scoped workspace and executor bindings when switching runs', () => {
    const bound = applyWorkSurfaceEvent(
      createEmptyWorkSurface('session-1', 'run-1'),
      {
        type: 'workspace_bound',
        workspace: {
          kind: 'server_sandbox',
          display_name: 'Server sandbox',
          cwd: '/tmp/astra-workspaces/session-1',
          authority: 'read_write',
          fallback_policy: 'disabled',
        },
        executor: {
          kind: 'server_local',
          executor_id: 'server-local',
          display_name: 'Server sandbox',
          transport: 'server_local',
          status: 'online',
        },
      },
    );

    const next = resetWorkSurfaceForRun(bound, {
      sessionId: 'session-1',
      runId: 'run-2',
    });

    expect(next.workspace).toBeUndefined();
    expect(next.executor).toBeUndefined();
    expect(next.runId).toBe('run-2');
  });

  it('hydrates from tasks and current run events', () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface(), {
      sessionId: 'session-1',
      runId: 'run-1',
      tasks: [task],
      events: [
        {
          type: 'agent_spawned',
          agent_id: 'agent-1',
          run_id: 'run-2',
          parent_run_id: 'run-1',
          agent_type: 'reviewer',
          description: 'Audit changes',
          workspace: {
            kind: 'edge_workspace',
            cwd: '/Users/xupeng/github/astra',
            fallback_policy: 'disabled',
          },
          executor: {
            kind: 'edge_agent',
            executor_id: 'edge-macbook-1',
            transport: 'edge_ws',
          },
          transport: 'edge_ws',
          fallback_policy: 'disabled',
        },
        {
          type: 'agent_completed',
          agent_id: 'agent-1',
          result_summary: 'No blockers',
          total_tool_calls: 3,
          duration_ms: 42,
        },
      ],
      generatedAt: '2026-06-10T00:00:00.000Z',
    });

    expect(state.tasks).toEqual([task]);
    expect(state.runId).toBe('run-1');
    expect(state.agents).toHaveLength(1);
    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-1',
      agentType: 'reviewer',
      description: 'Audit changes',
      resultSummary: 'No blockers',
      status: 'completed',
      totalToolCalls: 3,
      durationMs: 42,
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/xupeng/github/astra',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });
    expect(state.agents[0].events?.map((event) => event.label)).toEqual([
      'Spawned',
      'Completed',
    ]);
  });

  it('treats hydration as the authoritative tool and agent projection for the run', () => {
    let state = applyWorkSurfaceEvent(
      createEmptyWorkSurface('session-1', 'run-old'),
      {
        type: 'tool_call_start',
        call_id: 'call-old',
        tool: 'bash',
      },
    );
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_spawned',
      agent_id: 'agent-old',
      run_id: 'run-old',
      agent_type: 'reviewer',
    });

    const hydrated = hydrateWorkSurface(state, {
      sessionId: 'session-1',
      runId: 'run-new',
      tasks: [task],
      events: [
        {
          type: 'tool_call_start',
          call_id: 'call-new',
          tool: 'rg',
        },
      ],
    });

    expect(hydrated.runId).toBe('run-new');
    expect(hydrated.tools.map((item) => item.callId)).toEqual(['call-new']);
    expect(hydrated.agents).toEqual([]);
  });

  it('keeps partial hydration warnings separate from fatal errors', () => {
    const state = hydrateWorkSurface(createEmptyWorkSurface('session-1'), {
      sessionId: 'session-1',
      runId: 'run-1',
      tasks: [task],
      events: [],
      warnings: ['Run activity is temporarily unavailable.'],
    });

    expect(state.error).toBeNull();
    expect(state.warnings).toEqual([
      'Run activity is temporarily unavailable.',
    ]);
    expect(state.tasks).toEqual([task]);
  });

  it('updates subagents from live current-protocol progress events', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: 'agent_delegated',
      agent_id: 'agent-2',
      task: 'Explore task API',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-2',
      status: 'tool_executing',
      tool_name: 'grep',
      turn: 2,
      max_turns: 5,
      total_tool_calls: 4,
      total_tokens: { prompt: 100, completion: 20 },
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-2',
      description: 'Explore task API',
      status: 'tool_executing',
      toolName: 'grep',
      turn: 2,
      maxTurns: 5,
      totalToolCalls: 4,
      totalPromptTokens: 100,
      totalCompletionTokens: 20,
    });
    expect(state.agents[0].events?.at(-1)).toMatchObject({
      label: 'Running grep',
      detail: 'turn 2/5, 4 tools, 120 tokens',
      tone: 'running',
    });
  });

  it('keeps model wait and turn progress visible on subagent cards', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: 'agent_spawned',
      agent_id: 'agent-live',
      run_id: 'child-run',
      parent_run_id: 'root-run',
      agent_type: 'code-review',
      description: 'Review changes',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-live',
      status: 'busy',
      activity: 'executing',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-live',
      status: 'llm_call_started',
      turn: 1,
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-live',
      status: 'llm_call_completed',
      turn: 1,
      ttft_ms: 17,
      duration_ms: 91,
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-live',
      status: 'turn_completed',
      turn: 1,
      tool_calls_this_turn: 0,
      activity: 'summarized',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_progress',
      agent_id: 'agent-live',
      status: 'permission_denied',
      tool_name: 'bash',
      reason: 'approval required',
      turn: 2,
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-live',
      status: 'permission_denied',
      reason: 'approval required',
      toolName: 'bash',
    });
    expect(state.agents[0].events?.map((event) => event.label)).toEqual([
      'Spawned',
      'Working',
      'Waiting for model',
      'Model responded',
      'Turn completed',
      'Permission denied',
    ]);
    expect(state.agents[0].events?.at(-1)).toMatchObject({
      detail: 'bash, approval required, turn 2',
      tone: 'danger',
    });
  });

  it('streams agent live output and tool lifecycle into the subagent card', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: 'agent_spawned',
      agent_id: 'agent-live',
      run_id: 'child-run',
      description: 'Review changes',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_live_event',
      agent_id: 'agent-live',
      event_kind: 'output_delta',
      content: 'hello ',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_live_event',
      agent_id: 'agent-live',
      event_kind: 'output_delta',
      content: 'world',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_live_event',
      agent_id: 'agent-live',
      event_kind: 'tool_started',
      name: 'bash',
      description: 'cargo test',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_live_event',
      agent_id: 'agent-live',
      event_kind: 'tool_completed',
      name: 'bash',
      status: 'ok',
      output_summary: 'tests passed',
    });
    state = applyWorkSurfaceEvent(state, {
      type: 'agent_live_event',
      agent_id: 'agent-live',
      event_kind: 'agent_terminated',
      termination: 'completed',
      duration_ms: 42,
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-live',
      status: 'completed',
      toolName: 'bash',
      durationMs: 42,
    });
    expect(state.agents[0].events?.map((event) => event.label)).toEqual([
      'Spawned',
      'Output',
      'Running bash',
      'bash ok',
      'completed',
    ]);
    expect(state.agents[0].events?.[1]).toMatchObject({
      detail: 'hello world',
      tone: 'running',
    });
  });

  it('keeps execution metadata when live agent output arrives before spawn progress', () => {
    let state = applyWorkSurfaceEvent(createEmptyWorkSurface(), {
      type: 'agent_live_event',
      agent_id: 'agent-live-first',
      event_kind: 'output_delta',
      content: 'reviewing',
      workspace: {
        kind: 'edge_workspace',
        display_name: 'MacBook Pro',
        cwd: '/Users/test/project',
        authority: 'read_write',
        fallback_policy: 'disabled',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
        display_name: 'MacBook Pro',
        transport: 'edge_ws',
        status: 'online',
      },
      transport: 'edge_ws',
      fallback_policy: 'disabled',
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-live-first',
      status: 'running',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/test/project',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
      },
      transport: 'edge_ws',
      fallbackPolicy: 'disabled',
    });

    state = applyWorkSurfaceEvent(state, {
      type: 'agent_spawned',
      agent_id: 'agent-live-first',
      run_id: 'child-run-live-first',
      parent_run_id: 'parent-run',
      agent_type: 'code-review',
      description: 'Review branch',
    });

    expect(state.agents[0]).toMatchObject({
      agentId: 'agent-live-first',
      runId: 'child-run-live-first',
      parentRunId: 'parent-run',
      agentType: 'code-review',
      description: 'Review branch',
      workspace: {
        kind: 'edge_workspace',
        cwd: '/Users/test/project',
      },
      executor: {
        kind: 'edge_agent',
        executor_id: 'edge-macbook-1',
      },
    });
  });
});
