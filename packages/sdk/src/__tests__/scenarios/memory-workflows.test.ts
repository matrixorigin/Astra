import { AstraClient } from '../../client';
import type { MemoryEntry, MemorySearchResult } from '../../types';

let originalFetch: typeof globalThis.fetch;
beforeEach(() => {
  originalFetch = globalThis.fetch;
});
afterEach(() => {
  globalThis.fetch = originalFetch;
});

function jsonResponse(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: () => Promise.resolve(typeof body === 'string' ? body : JSON.stringify(body)),
    json: () => Promise.resolve(body),
    headers: new Headers(),
  } as unknown as Response;
}

describe('scenarios / memory workflows', () => {
  it('stores session-scoped memories, searches by score order, retrieves topK, and purges by topic', async () => {
    const stored: Array<MemoryEntry & { id: string }> = [];
    const fetchImpl = jest.fn().mockImplementation((url: string, init?: RequestInit) => {
      const path = new URL(url).pathname;
      const body = init?.body ? JSON.parse(String(init.body)) : {};

      if (path === '/memory/store') {
        const id = `mem-${stored.length + 1}`;
        stored.push({ id, ...(body as MemoryEntry) });
        return Promise.resolve(jsonResponse({ id }));
      }

      if (path === '/memory/search') {
        expect(body.top_k).toBe(3);
        const results: MemorySearchResult[] = stored
          .filter((m) => m.content.includes('PKCE') || m.content.includes('OAuth'))
          .map((m) => ({
            id: m.id,
            content: m.content,
            score: m.content.includes('PKCE') ? 0.96 : 0.72,
            memory_type: m.memory_type,
          }))
          .sort((a, b) => b.score - a.score)
          .slice(0, body.top_k);
        return Promise.resolve(jsonResponse(results));
      }

      if (path === '/memory/retrieve') {
        expect(body.top_k).toBe(1);
        const results: MemorySearchResult[] = stored
          .filter((m) => m.session_id === 'sess-memory-1')
          .map((m) => ({ id: m.id, content: m.content, score: 0.9, memory_type: m.memory_type }))
          .slice(0, body.top_k);
        return Promise.resolve(jsonResponse(results));
      }

      if (path === '/memory/purge') {
        expect(body.topic).toBe('OAuth');
        for (let i = stored.length - 1; i >= 0; i--) {
          if (stored[i].content.includes('OAuth') || stored[i].content.includes('PKCE')) {
            stored.splice(i, 1);
          }
        }
        return Promise.resolve(jsonResponse({}));
      }

      throw new Error(`unexpected fetch ${url}`);
    });
    globalThis.fetch = fetchImpl;

    const client = new AstraClient({ baseUrl: 'http://localhost:8000', accessToken: 'token' });

    await expect(
      client.memoryStore({
        content: 'OAuth migration must use PKCE and keep sessions intact',
        memory_type: 'semantic',
        session_id: 'sess-memory-1',
        trust_tier: 'T2',
      }),
    ).resolves.toEqual({ id: 'mem-1' });
    await client.memoryStore({
      content: 'Cloud-edge regression suite covers duplicate tool callbacks',
      memory_type: 'procedural',
      session_id: 'sess-memory-2',
    });

    const search = await client.memorySearch('OAuth PKCE', 3);
    expect(search.map((m) => m.id)).toEqual(['mem-1']);
    expect(search[0].score).toBeGreaterThan(0.9);

    const retrieved = await client.memoryRetrieve('session scoped OAuth facts', 1);
    expect(retrieved).toHaveLength(1);
    expect(retrieved[0].content).toContain('PKCE');

    await client.memoryPurge('OAuth');
    const afterPurge = await client.memorySearch('OAuth PKCE', 3);
    expect(afterPurge).toEqual([]);
  });
});
