/** @vitest-environment jsdom */
import { afterEach, expect, test, vi } from 'vitest';
import { act, cleanup, renderHook, waitFor } from '@testing-library/react';
import { AstraClient } from '../client';
import { useAstraRun } from '../hooks';

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const sse = (events: unknown[]) => events.map(e => `data: ${JSON.stringify(e)}\n\n`).join('');

test('Work retains explicit validation of an invalid event envelope', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(sse([
    {}, { type: 'run_finished', run_id: 'run-1', status: 'completed' },
  ]))));
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'token' });
  const events: unknown[] = [];
  const stream = client.continueWorkBranch('work-1', 'branch-1', {
    requestId: 'request-1', attachmentId: 'attachment-1', message: 'continue',
  }, { onEvent: event => events.push(event) });
  try {
    await waitFor(() => expect(events).toContainEqual(expect.objectContaining({
      type: 'error', code: 'WORK_PROTOCOL_ERROR', retryable: false,
    })));
  } finally { stream.close(); }
});

test.each([
  [{}, 'Invalid SSE event envelope'],
  [{ type: 'error', message: 'Run replay could not continue', code: 'UNAVAILABLE', error_kind: 'stream_transport', retryable: true }, 'Run replay could not continue'],
])('an incomplete replay retains its cursor (%j)', async (failure, message) => {
  const source = [{ type: 'text_delta', content: 'a', index: 7 }, failure];
  const indexes: number[] = [];
  vi.stubGlobal('fetch', vi.fn().mockImplementation(async (input: string) => {
    const url = new URL(input);
    if (url.pathname.endsWith('/stream')) {
      const index = Number(url.searchParams.get('last_index'));
      expect(url.searchParams.get('replay_only')).toBe('true');
      indexes.push(index);
      return new Response(sse(source.slice(index)));
    }
    return Response.json({ status: 'running', events_count: 8 });
  }));
  const client = new AstraClient({ baseUrl: 'https://example.test', accessToken: 'token' });
  const { result } = renderHook(() => useAstraRun({ client, runId: 'run-1', pollIntervalMs: 60000 }));
  await waitFor(() => expect(result.current.isPolling).toBe(false));
  expect(result.current.error).toMatchObject({ ...failure, message });
  expect(result.current.events).toEqual([]);
  await act(async () => { await result.current.refresh(); });
  expect(indexes).toEqual([0, 0]);
  expect(result.current.events).toEqual([]);
});
