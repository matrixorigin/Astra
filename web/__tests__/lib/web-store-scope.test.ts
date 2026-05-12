jest.mock('@/lib/runtime-config', () => ({
  getRuntimeConfig: jest.fn(),
}));

import { createChatWithMessage, listChats } from '@/lib/api/web-store';
import { getRuntimeConfig } from '@/lib/runtime-config';

const mockGetRuntimeConfig = getRuntimeConfig as jest.MockedFunction<typeof getRuntimeConfig>;

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText: status >= 200 && status < 300 ? 'OK' : 'Error',
    json: jest.fn().mockResolvedValue(body),
  };
}

describe('web store user scoping', () => {
  beforeEach(() => {
    globalThis.__astraWebStores = undefined;
    mockGetRuntimeConfig.mockResolvedValue({
      mode: 'live',
      source: 'cookie',
      apiUrl: 'http://runtime.test',
      accessToken: 'test-token',
      refreshToken: undefined,
      demoMode: false,
      hasAccessToken: true,
      hasRefreshToken: false,
      maskedAccessToken: 'test-token',
      message: 'test runtime',
    });
    globalThis.fetch = jest.fn().mockResolvedValue(jsonResponse({
      session_id: 'session-user-a',
      user_id: 'user-a',
      title: 'test',
      metadata: {},
      status: 'active',
      created_at: new Date().toISOString(),
      updated_at: null,
    }, 201));
  });

  it('does not expose one user chat list to another authenticated user', async () => {
    const uniqueMessage = `private scoped prompt ${crypto.randomUUID()}`;

    const result = await createChatWithMessage('user-a', {
      message: uniqueMessage,
      model: 'sonnet-4.6-adaptive',
      options: {
        webSearch: false,
        thinking: true,
        activeSkills: ['skill-creator'],
      },
      projectId: null,
    });

    expect(result.chatId).toBe('session-user-a');
    expect((await listChats('user-a', { q: uniqueMessage })).items).toHaveLength(1);
    expect((await listChats('user-b', { q: uniqueMessage })).items).toHaveLength(0);
  });
});
