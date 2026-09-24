// @vitest-environment node
import { NextRequest } from 'next/server';
import { GET, POST } from '@/app/api/sessions/route';
import { requireRuntimeClient, RuntimeClientError } from '@/lib/runtime-client';

vi.mock('@/lib/runtime-client', async (original) => ({ ...await original<typeof import('@/lib/runtime-client')>(), requireRuntimeClient: vi.fn() }));

beforeEach(() => vi.resetAllMocks());

it('lists only through the authenticated session API and forwards the complete cursor', async () => {
  const listRuntimeSessions = vi.fn().mockResolvedValue({ sessions: [], next_cursor: null });
  vi.mocked(requireRuntimeClient).mockResolvedValue({ sdk: { listRuntimeSessions } } as never);
  const response = await GET(new NextRequest('http://web.test/api/sessions?after_updated_at=stamp&after_session_id=session'));
  expect(response.status).toBe(200);
  expect(requireRuntimeClient).toHaveBeenCalledWith(expect.objectContaining({ auth: 'required' }));
  expect(listRuntimeSessions).toHaveBeenCalledWith({ limit: 50, cursor: { updated_at: 'stamp', session_id: 'session' } });
  expect((await GET(new NextRequest('http://web.test/api/sessions?after_session_id=session'))).status).toBe(400);
  expect(listRuntimeSessions).toHaveBeenCalledTimes(1);
});

it('rejects unauthenticated creation without falling back to a local session', async () => {
  vi.mocked(requireRuntimeClient).mockRejectedValue(new RuntimeClientError({ operation: 'create session', path: '/sessions', status: 401, detail: 'Login required' }));
  const response = await POST(new NextRequest('http://web.test/api/sessions', { method: 'POST', body: JSON.stringify({ title: 'Skill task' }) }));
  expect(response.status).toBe(401);
});
