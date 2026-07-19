// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { GET } from "@/app/api/models/route";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

function runtimeClient(
  accessToken: string | undefined,
  listModels: ReturnType<typeof vi.fn>,
) {
  return {
    config: { accessToken },
    sdk: { listModels },
  };
}

describe("/api/models", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("exposes active runtime models using model_id as id and name as display label", async () => {
    const listModels = vi.fn().mockResolvedValue([
      {
        model_id: "row-flash",
        name: "deepseek-v4-flash-anthropic",
        provider: "anthropic",
        description: "Flash endpoint",
        architecture: "deepseek-v4",
        context_window: 128_000,
        thinking_capability: { kind: "native" },
        is_active: true,
      },
      {
        model_id: "row-only",
        provider: "openai",
        is_active: true,
      },
      {
        model_id: "row-inactive",
        name: "inactive-model",
        provider: "openai",
        is_active: false,
      },
    ]);
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", listModels) as never,
    );

    const response = await GET();
    const payload = await response.json();

    expect(payload.source).toBe("astra");
    expect(payload.status).toBe("ready");
    expect(payload.items).toHaveLength(2);
    expect(payload.items[0]).toMatchObject({
      id: "row-flash",
      name: "deepseek-v4-flash-anthropic",
      tier: "included",
    });
    expect(payload.items[0].subtitle).toContain("anthropic");
    expect(payload.items[0].subtitle).toContain("128k context");
    expect(payload.items[1]).toMatchObject({
      id: "row-only",
      name: "row-only",
      tier: "included",
    });
    expect(payload.items[1].subtitle).toContain("openai");
  });

  it("surfaces Model Access failure instead of inventing static Offerings", async () => {
    mockRequireRuntimeClient.mockRejectedValue(new Error("not configured"));

    const response = await GET();
    const payload = await response.json();

    expect(response.status).toBe(503);
    expect(payload).toEqual({
      error: "model_access_unavailable",
      detail: "not configured",
      action: "sign_in_or_retry",
    });
  });
});
