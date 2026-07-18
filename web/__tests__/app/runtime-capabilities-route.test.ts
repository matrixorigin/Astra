// @vitest-environment node

vi.mock('@/lib/runtime-client', () => ({
  RuntimeClientError: class RuntimeClientError extends Error {
    status: number;

    constructor(context: { detail: string; status?: number }) {
      super(context.detail);
      this.status = context.status ?? 502;
    }
  },
  requireRuntimeClient: vi.fn(),
  runtimeErrorDetail: (error: unknown) =>
    error instanceof Error ? error.message : String(error),
}));

import { PATH_RUNTIME_CAPABILITIES } from '@astra/sdk';
import { GET } from '@/app/api/runtime/capabilities/route';
import {
  RuntimeClientError,
  requireRuntimeClient,
} from '@/lib/runtime-client';

const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

describe('/api/runtime/capabilities route', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('preserves the authentication failure instead of probing anonymously', async () => {
    mockRequireRuntimeClient.mockRejectedValue(
      new RuntimeClientError({
        operation: 'discover runtime capabilities',
        path: '/runtime/capabilities',
        status: 401,
        detail: 'Runtime authentication is missing.',
      }),
    );

    const response = await GET();

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      error: 'Runtime authentication is missing.',
    });
  });

  it('returns an explicit gateway failure when capability discovery fails', async () => {
    const get = vi
      .fn()
      .mockRejectedValue(new Error('capability registry unavailable'));
    mockRequireRuntimeClient.mockResolvedValue({ get } as never);

    const response = await GET();

    expect(response.status).toBe(502);
    expect(await response.json()).toEqual({
      error: 'capability registry unavailable',
    });
  });

  it('proxies the authenticated runtime capability contract', async () => {
    const payload = {
      tools: ['bash', 'read_file'],
      providers: [],
    };
    const get = vi.fn().mockResolvedValue(payload);
    mockRequireRuntimeClient.mockResolvedValue({ get } as never);

    const response = await GET();

    expect(mockRequireRuntimeClient).toHaveBeenCalledWith({
      auth: 'required',
      operation: 'discover runtime capabilities',
    });
    expect(get).toHaveBeenCalledWith(PATH_RUNTIME_CAPABILITIES, {
      auth: 'required',
      operation: 'discover runtime capabilities',
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual(payload);
  });
});
