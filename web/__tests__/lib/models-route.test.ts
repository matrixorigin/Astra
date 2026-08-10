// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { GET } from "@/app/api/models/route";
import { requireRuntimeClient } from "@/lib/runtime-client";

const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

function runtimeClient(
  accessToken: string | undefined,
  getModelAccess: ReturnType<typeof vi.fn>,
) {
  return {
    config: { accessToken },
    sdk: { getModelAccess },
  };
}

describe("/api/models", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("exposes effective Offerings with their Model Access placement", async () => {
    const getModelAccess = vi.fn().mockResolvedValue({
      accesses: [{
        id: "self-hosted",
        kind: "self_hosted",
        label: "Self-hosted",
        execution_placement: "server",
        status: "ready",
        reason: null,
        usable: true,
        retry_after_seconds: null,
        available_model_count: 2,
        actions: [],
      }],
      default_offering_id: "row-only",
      default_resolution: {
        state: "selected",
        offering_id: "row-only",
        source: "astra",
        scope: "effective_catalog",
      },
      catalog_revision: "sha256:catalog",
      observed_at: "2026-07-20T00:00:00Z",
      offerings: [{
        offering_id: "row-flash",
        access_id: "self-hosted",
        access_kind: "self_hosted",
        access_label: "Self-hosted",
        execution_placement: "server",
        name: "deepseek-v4-flash-anthropic",
        provider: "anthropic",
        description: "Flash endpoint",
        architecture: "deepseek-v4",
        context_window: 128_000,
        thinking_capability: "native_only",
        is_active: true,
      },
      {
        offering_id: "row-only",
        access_id: "self-hosted",
        access_kind: "self_hosted",
        access_label: "Self-hosted",
        execution_placement: "server",
        name: "row-only",
        provider: "openai",
        description: null,
        architecture: null,
        context_window: 8192,
        max_completion_tokens: null,
        is_active: true,
      },
    ]});
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", getModelAccess) as never,
    );

    const response = await GET();
    const payload = await response.json();

    expect(payload.source).toBe("astra");
    expect(payload.items).toHaveLength(2);
    expect(payload.items[0]).toMatchObject({
      id: "row-flash",
      name: "deepseek-v4-flash-anthropic",
      tier: "included",
    });
    expect(payload.items[0].subtitle).toContain("Self-hosted");
    expect(payload.items[0].subtitle).toContain("Runs on server");
    expect(payload.items[0].subtitle).toContain("128k context");
    expect(payload.items[1]).toMatchObject({
      id: "row-only",
      name: "row-only",
      tier: "included",
    });
    expect(payload.items[1].subtitle).toContain("Self-hosted");
    expect(payload.defaultOfferingId).toBe("row-only");
    expect(payload.catalogRevision).toBe("sha256:catalog");
    expect(payload.observedAt).toBe("2026-07-20T00:00:00Z");
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
