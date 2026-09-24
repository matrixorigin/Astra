import { requestJson } from '@/lib/api/request';
import { createSkillifyRun } from '@/lib/api/harnesses';

describe('request deadlines', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal('fetch', vi.fn((_path: string, init: RequestInit) => new Promise((resolve, reject) => {
      const timer = setTimeout(() => resolve({ ok: true, json: async () => ({ run_id: 'created' }) }), 31_000);
      init.signal?.addEventListener('abort', () => {
        clearTimeout(timer);
        reject(new DOMException('Aborted', 'AbortError'));
      }, { once: true });
    })));
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('lets synchronous Skillify creation finish after thirty seconds', async () => {
    const result = createSkillifyRun({ session_ids: ['source'] });
    await vi.advanceTimersByTimeAsync(31_000);
    await expect(result).resolves.toEqual({ run_id: 'created' });
  });

  it('enforces an explicitly supplied Evaluation deadline', async () => {
    const result = requestJson('/api/evaluations/experiments/id/report', { timeoutMs: 100 });
    const rejected = expect(result).rejects.toMatchObject({ name: 'AbortError' });
    await vi.advanceTimersByTimeAsync(100);
    await rejected;
  });

  it('preserves caller cancellation without a deadline', async () => {
    const controller = new AbortController();
    const result = requestJson('/api/example', { signal: controller.signal });
    const rejected = expect(result).rejects.toMatchObject({ name: 'AbortError' });
    controller.abort();
    await rejected;
  });
});
