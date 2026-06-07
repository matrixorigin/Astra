/**
 * @jest-environment node
 *
 * Tests for resolveBackendModelName cache behavior.
 */

import { WebRuntimeClient } from '@/lib/runtime-client';
import { resolveBackendModelName } from '@/lib/api/web-store';

jest.mock('@/lib/runtime-client', () => {
  const actual = jest.requireActual('@/lib/runtime-client');
  return { ...actual, WebRuntimeClient: jest.fn() };
});

const MockedWRC = WebRuntimeClient as jest.Mock;

let tokenCounter = 0;

function makeRuntime(overrides: Partial<{ accessToken: string | null }> = {}) {
  const mockListModels = jest.fn();
  const mockConfig = {
    mode: 'live' as const,
    apiUrl: 'http://test.test',
    accessToken: 'accessToken' in overrides ? overrides.accessToken! : `token-${++tokenCounter}`,
    refreshToken: null as string | null,
    tokenExpiresAtMs: null as number | null,
  };

  MockedWRC.mockImplementation(() => ({
    config: mockConfig,
    sdk: { listModels: mockListModels },
  }));

  const client = new WebRuntimeClient(mockConfig as any);
  return { client, mockListModels };
}

describe('resolveBackendModelName', () => {
  it('resolves model name on first call via listModels', async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: 'sonnet-4.6-adaptive', name: 'Sonnet 4.6' },
    ]);

    const result = await resolveBackendModelName(client, 'sonnet-4.6-adaptive');
    expect(result).toBe('Sonnet 4.6');
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it('caches listModels — second call does not call listModels again', async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: 'sonnet-4.6-adaptive', name: 'Sonnet 4.6' },
    ]);

    await resolveBackendModelName(client, 'sonnet-4.6-adaptive');
    await resolveBackendModelName(client, 'sonnet-4.6-adaptive');
    // With cache: only 1 call
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it('returns original model string when listModels does not match', async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([{ model_id: 'opus-4.7', name: 'Opus 4.7' }]);

    const result = await resolveBackendModelName(client, 'unknown-model-xyz');
    expect(result).toBe('unknown-model-xyz');
  });

  it('returns undefined immediately when no model provided', async () => {
    const { client, mockListModels } = makeRuntime();

    const result = await resolveBackendModelName(client, undefined);
    expect(result).toBeUndefined();
    expect(mockListModels).not.toHaveBeenCalled();
  });

  it('returns original model on listModels error (graceful degradation)', async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockRejectedValue(new Error('Network error'));

    const result = await resolveBackendModelName(client, 'sonnet-4.6-adaptive');
    expect(result).toBe('sonnet-4.6-adaptive');
  });

  it('skips listModels when no accessToken', async () => {
    const { client, mockListModels } = makeRuntime({ accessToken: null });

    const result = await resolveBackendModelName(client, 'sonnet-4.6-adaptive');
    expect(result).toBe('sonnet-4.6-adaptive');
    expect(mockListModels).not.toHaveBeenCalled();
  });
});
