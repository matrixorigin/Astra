import { AstraClient, chatRequestToWire } from '../../client';
import type { StreamEvent } from '../../types';

function streamFrom(chunks: string[]) {
  const enc = new TextEncoder();
  let i = 0;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (i < chunks.length) {
        controller.enqueue(enc.encode(chunks[i]));
        i++;
      } else {
        controller.close();
      }
    },
  });
}

function okSseResponse(): Response {
  return {
    ok: true,
    status: 200,
    body: streamFrom(['data: {"type":"turn_complete"}\n\n']),
    headers: new Headers(),
  } as unknown as Response;
}

async function nextTick(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

describe('scenarios / multi-turn context wire contract', () => {
  it('preserves session, context, planning, budget, model, tools, skills, and edge routing fields', () => {
    const wire = chatRequestToWire({
      message: 'continue from the last failed migration',
      sessionId: 'sess-multi-1',
      agentId: 'agent-planner',
      model: 'claude-sonnet-4.6',
      context: {
        previousTurns: [
          { role: 'user', content: 'migrate auth table' },
          { role: 'assistant', content: 'migration failed at step 3' },
        ],
        memoryHints: ['use PKCE', 'do not drop sessions'],
      },
      explain: true,
      planSubtaskId: 'subtask-db-3',
      isPlanSubtask: true,
      edgeExecutorId: 'edge-shanghai-1',
      capabilities: ['shell', 'filesystem', 'edge-tools'],
      allowSkills: ['db-migration', 'regression-tests'],
      allowTools: ['read_file', 'grep', 'bash'],
      executionBudget: { initialTurns: 2, hardTurnLimit: 6 },
      skillSearch: { dynamicSurface: true, minCatalogSize: 8, surfaceCap: 12 },
    });

    expect(wire).toEqual({
      message: 'continue from the last failed migration',
      session_id: 'sess-multi-1',
      agent_id: 'agent-planner',
      model: 'claude-sonnet-4.6',
      context: {
        previousTurns: [
          { role: 'user', content: 'migrate auth table' },
          { role: 'assistant', content: 'migration failed at step 3' },
        ],
        memoryHints: ['use PKCE', 'do not drop sessions'],
      },
      explain: true,
      plan_subtask_id: 'subtask-db-3',
      is_plan_subtask: true,
      edge_executor_id: 'edge-shanghai-1',
      capabilities: ['shell', 'filesystem', 'edge-tools'],
      allow_skills: ['db-migration', 'regression-tests'],
      allow_tools: ['read_file', 'grep', 'bash'],
      execution_budget: { initial_turns: 2, hard_turn_limit: 6 },
      skill_search: { dynamic_surface: true, min_catalog_size: 8, surface_cap: 12 },
    });
  });

  it('streamChat sends consecutive turns with the same session id and updated context', async () => {
    const fetchImpl = jest.fn().mockResolvedValue(okSseResponse());
    globalThis.fetch = fetchImpl;

    const client = new AstraClient({
      baseUrl: 'http://localhost:8000',
      accessToken: 'token',
    });
    const events: StreamEvent[] = [];

    const first = client.streamChat(
      {
        message: 'turn 1: inspect the cache regression',
        sessionId: 'sess-ctx',
        context: { turn: 1, facts: ['cache_read_tokens unexpectedly zero'] },
        executionBudget: { initialTurns: 1, hardTurnLimit: 4 },
      },
      { onEvent: (e) => events.push(e) },
    );
    await nextTick();
    first.close();

    const second = client.streamChat(
      {
        message: 'turn 2: apply the fix',
        sessionId: 'sess-ctx',
        context: {
          turn: 2,
          facts: ['cache_read_tokens unexpectedly zero', 'tool schema changed mid-session'],
          priorRunId: 'run-1',
        },
        executionBudget: { initialTurns: 2, hardTurnLimit: 4 },
      },
      { onEvent: (e) => events.push(e) },
    );
    await nextTick();
    second.close();

    expect(fetchImpl).toHaveBeenCalledTimes(2);
    const firstBody = JSON.parse(fetchImpl.mock.calls[0][1].body);
    const secondBody = JSON.parse(fetchImpl.mock.calls[1][1].body);

    expect(firstBody.session_id).toBe('sess-ctx');
    expect(secondBody.session_id).toBe('sess-ctx');
    expect(firstBody.context).toEqual({
      turn: 1,
      facts: ['cache_read_tokens unexpectedly zero'],
    });
    expect(secondBody.context).toEqual({
      turn: 2,
      facts: ['cache_read_tokens unexpectedly zero', 'tool schema changed mid-session'],
      priorRunId: 'run-1',
    });
    expect(secondBody.execution_budget).toEqual({ initial_turns: 2, hard_turn_limit: 4 });
  });
});
