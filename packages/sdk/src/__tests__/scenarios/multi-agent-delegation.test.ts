import { AstraClient } from '../../client';
import { chatRunDelegatePath, chatRunDelegationsPath } from '../../paths';
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

function jsonResponse(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: () => Promise.resolve(JSON.stringify(body)),
    json: () => Promise.resolve(body),
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

describe('scenarios / multi-agent delegation', () => {
  it('streams nested delegation lifecycle events in causal order', async () => {
    const chunks = [
      'data: {"type":"agent_delegated","agent_id":"researcher","task":"map context"}\n\n',
      'data: {"type":"agent_spawned","agent_id":"researcher","run_id":"run-child-1","parent_run_id":"run-parent","agent_type":"explore","description":"Map context"}\n\n',
      'data: {"type":"agent_progress","agent_id":"researcher","status":"running","description":"reading files","turn":1,"max_turns":3}\n\n',
      'data: {"type":"agent_delegated","agent_id":"tester","task":"verify regression"}\n\n',
      'data: {"type":"agent_spawned","agent_id":"tester","run_id":"run-child-2","parent_run_id":"run-child-1","agent_type":"task","description":"Verify regression"}\n\n',
      'data: {"type":"agent_completed","agent_id":"tester","status":"completed","result_summary":"tests passed","total_tool_calls":2}\n\n',
      'data: {"type":"agent_completed","agent_id":"researcher","status":"completed","result_summary":"context mapped","total_tool_calls":5}\n\n',
      'data: {"type":"turn_complete"}\n\n',
    ];
    globalThis.fetch = jest.fn().mockResolvedValue({
      ok: true,
      status: 200,
      body: streamFrom(chunks),
      headers: new Headers(),
    } as unknown as Response);

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 'token' });
    const events: StreamEvent[] = [];
    const sse = client.streamChat({ message: 'coordinate agents', sessionId: 'sess-agents' }, {
      onEvent: (event) => events.push(event),
    });
    await nextTick();
    sse.close();

    expect(events.map((event) => event.type)).toEqual([
      'agent_delegated',
      'agent_spawned',
      'agent_progress',
      'agent_delegated',
      'agent_spawned',
      'agent_completed',
      'agent_completed',
      'turn_complete',
    ]);
    expect(events[1]).toMatchObject({
      type: 'agent_spawned',
      agent_id: 'researcher',
      run_id: 'run-child-1',
      parent_run_id: 'run-parent',
    });
    expect(events[4]).toMatchObject({
      type: 'agent_spawned',
      agent_id: 'tester',
      run_id: 'run-child-2',
      parent_run_id: 'run-child-1',
    });
    expect(events[6]).toMatchObject({
      type: 'agent_completed',
      agent_id: 'researcher',
      status: 'completed',
      total_tool_calls: 5,
    });
  });

  it('delegates, lists, pauses, and resumes a parent run with stable paths and bodies', async () => {
    const fetchImpl = jest.fn().mockImplementation((url: string, init?: RequestInit) => {
      const path = new URL(url).pathname;
      if (path === chatRunDelegatePath('run-parent')) {
        expect(init?.method).toBe('POST');
        expect(JSON.parse(String(init?.body))).toEqual({
          delegation_id: 'del-1',
          parent_run_id: 'run-parent',
          task: 'validate cloud-edge rollback',
          pattern: { type: 'fan_out', agents: ['agent-edge', 'agent-tester'] },
          user_id: 'user-1',
          depth: 1,
          context: { session_id: 'sess-agents', priority: 'high' },
        });
        return Promise.resolve(jsonResponse({
          delegation_id: 'del-1',
          status: 'running',
          agent_results: [],
          aggregated_output: null,
          total_prompt_tokens: 0,
          total_completion_tokens: 0,
          total_tool_calls: 0,
        }));
      }
      if (path === chatRunDelegationsPath('run-parent')) {
        return Promise.resolve(jsonResponse({
          parent_run_id: 'run-parent',
          sub_run_ids: ['run-child-a', 'run-child-b'],
        }));
      }
      if (path === '/chat/runs/run-parent/delegations/pause') {
        return Promise.resolve(jsonResponse({ parent_run_id: 'run-parent', affected: 2 }));
      }
      if (path === '/chat/runs/run-parent/delegations/resume') {
        return Promise.resolve(jsonResponse({ parent_run_id: 'run-parent', affected: 2 }));
      }
      throw new Error(`unexpected fetch ${url}`);
    });
    globalThis.fetch = fetchImpl;

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 'token' });

    await expect(client.delegateRun('run-parent', {
      delegation_id: 'del-1',
      parent_run_id: 'run-parent',
      task: 'validate cloud-edge rollback',
      pattern: { type: 'fan_out', agents: ['agent-edge', 'agent-tester'] },
      user_id: 'user-1',
      depth: 1,
      context: { session_id: 'sess-agents', priority: 'high' },
    })).resolves.toMatchObject({
      delegation_id: 'del-1',
      status: 'running',
    });

    const delegations = await client.listDelegations('run-parent');
    expect(delegations.parent_run_id).toBe('run-parent');
    expect(delegations.sub_run_ids).toEqual(['run-child-a', 'run-child-b']);

    await expect(client.pauseDelegations('run-parent')).resolves.toEqual({
      parent_run_id: 'run-parent',
      affected: 2,
    });
    await expect(client.resumeDelegations('run-parent')).resolves.toEqual({
      parent_run_id: 'run-parent',
      affected: 2,
    });
  });
});
