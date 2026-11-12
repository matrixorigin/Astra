import { afterEach, expect, test, vi } from 'vitest';
import { SSEClient } from '../sse-client';
import type { ConnectionState } from '../types';

afterEach(() => { vi.unstubAllGlobals(); vi.restoreAllMocks(); vi.useRealTimers(); });

test.each(['close', 'external-abort'])('%s stops delivery of already buffered events', async (cancel) => {
  const controller = new AbortController();
  const received: unknown[] = [];
  const transport = vi.fn().mockResolvedValue(new Response(
    'data: {"type":"text_delta","content":"first"}\n\n' +
    'data: {"type":"text_delta","content":"stale"}\n\n' +
    'data: {"type":"run_finished"}',
  ));
  const client = new SSEClient({
    url: 'https://example.test/stream', signal: controller.signal, transport,
    onEvent: (event) => {
      received.push(event);
      if (cancel === 'close') client.close(); else controller.abort();
    },
  });
  await client.connect();
  expect(received).toEqual([{ type: 'text_delta', content: 'first' }]);
});

for (const cancel of ['external-abort', 'close']) {
  test(`${cancel} settles connect during backoff without reconnecting`, async () => {
    vi.useFakeTimers();
    const controller = new AbortController();
    const states: ConnectionState[] = [];
    const fetch = vi.fn().mockImplementation((_url, init) => Promise.reject(
      init.signal.aborted ? new DOMException('Aborted', 'AbortError') : new Error('Connection failed'),
    ));
    vi.stubGlobal('fetch', fetch);
    const client = new SSEClient({
      url: 'https://example.test/stream', signal: controller.signal,
      retryDelayMs: 10_000, maxRetries: 2, onEvent: () => {},
      onStateChange: (state) => states.push(state),
    });
    let settled = false;
    const pending = client.connect().finally(() => { settled = true; });
    await vi.advanceTimersByTimeAsync(1);
    if (cancel === 'close') client.close(); else controller.abort();
    await vi.advanceTimersByTimeAsync(100);
    const settledPromptly = settled;
    const timersAfterCancel = vi.getTimerCount();
    await vi.runAllTimersAsync();
    await pending;
    client.close();
    expect(settledPromptly).toBe(true);
    expect(timersAfterCancel).toBe(0);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(states.filter((state) => state === 'connecting')).toHaveLength(1);
  });
}

test('an already aborted signal never starts a fetch or connecting state', async () => {
  const controller = new AbortController();
  controller.abort();
  const fetch = vi.fn();
  const state = vi.fn();
  vi.stubGlobal('fetch', fetch);
  const client = new SSEClient({
    url: 'https://example.test/stream', signal: controller.signal,
    onEvent: () => {}, onStateChange: state,
  });
  await client.connect();
  expect(fetch).not.toHaveBeenCalled();
  expect(state).not.toHaveBeenCalled();
});

test('retry delay still retries normally and releases external abort listeners', async () => {
  vi.useFakeTimers();
  const controller = new AbortController();
  const add = vi.spyOn(controller.signal, 'addEventListener');
  const remove = vi.spyOn(controller.signal, 'removeEventListener');
  const fetch = vi.fn().mockRejectedValueOnce(new Error('Connection failed'))
    .mockImplementation(() => Promise.resolve(new Response('data: {"type":"run_finished"}\n\n')));
  vi.stubGlobal('fetch', fetch);
  const received = vi.fn();
  const client = new SSEClient({
    url: 'https://example.test/stream', signal: controller.signal,
    retryDelayMs: 1_000, maxRetries: 1, onEvent: received,
  });
  const pending = client.connect();
  await vi.advanceTimersByTimeAsync(999);
  expect(fetch).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1);
  await pending;
  expect(fetch).toHaveBeenCalledTimes(2);
  expect(received).toHaveBeenLastCalledWith({ type: 'run_finished' });
  expect(remove.mock.calls.map(([event, listener]) => [event, listener]))
    .toEqual(add.mock.calls.map(([event, listener]) => [event, listener]));
  client.close();
  // A deliberate new connect still works after close.
  await client.connect();
  expect(fetch).toHaveBeenCalledTimes(3);
  expect(vi.getTimerCount()).toBe(0);
  client.close();
});

test('close from the error callback does not schedule backoff', async () => {
  vi.useFakeTimers();
  const fetch = vi.fn().mockRejectedValue(new Error('Connection failed'));
  vi.stubGlobal('fetch', fetch);
  const client = new SSEClient({
    url: 'https://example.test/stream', maxRetries: 2,
    onEvent: () => client.close(),
  });
  await client.connect();
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(vi.getTimerCount()).toBe(0);
});

test('close from the connecting callback prevents the fetch', async () => {
  const fetch = vi.fn();
  vi.stubGlobal('fetch', fetch);
  const client = new SSEClient({
    url: 'https://example.test/stream', onEvent: () => {},
    onStateChange: (state) => { if (state === 'connecting') client.close(); },
  });
  await client.connect();
  expect(fetch).not.toHaveBeenCalled();
});
