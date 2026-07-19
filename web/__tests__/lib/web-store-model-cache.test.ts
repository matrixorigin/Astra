// @vitest-environment node

import { WebRuntimeClient } from "@/lib/runtime-client/server";
import { resolveModelOfferingSelection } from "@/lib/api/web-store";
import { resetModelCacheForTests } from "@/lib/api/model-cache";

vi.mock("@/lib/runtime-client/server", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/lib/runtime-client/server")>();
  return { ...actual, WebRuntimeClient: vi.fn() };
});

const MockedWRC = WebRuntimeClient as ReturnType<typeof vi.fn>;

let tokenCounter = 0;

function makeRuntime(overrides: Partial<{ accessToken: string | null }> = {}) {
  const mockListModels = vi.fn();
  const mockConfig = {
    mode: "live" as const,
    apiUrl: "http://test.test",
    accessToken:
      "accessToken" in overrides
        ? overrides.accessToken!
        : `token-${++tokenCounter}`,
    refreshToken: null as string | null,
    tokenExpiresAtMs: null as number | null,
  };

  MockedWRC.mockImplementation(function () {
    return {
      config: mockConfig,
      sdk: { listModels: mockListModels },
    };
  });

  const client = new WebRuntimeClient(mockConfig as any);
  return { client, mockListModels };
}

describe("resolveModelOfferingSelection", () => {
  beforeEach(() => {
    resetModelCacheForTests();
  });

  it("admits an exact active Offering id on first call", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "sonnet-4.6-adaptive", name: "Sonnet 4.6" },
    ]);

    const result = await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    expect(result).toEqual({ offeringId: "sonnet-4.6-adaptive" });
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("caches listModels — second call does not call listModels again", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "sonnet-4.6-adaptive", name: "Sonnet 4.6" },
    ]);

    await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    // With cache: only 1 call
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("rejects a forged or stale Offering instead of falling back to a model name", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "opus-4.7", name: "Opus 4.7" },
    ]);

    await expect(
      resolveModelOfferingSelection(client, "unknown-model-xyz"),
    ).rejects.toThrow("Model Offering 'unknown-model-xyz' is not available");
  });

  it("rejects missing model before listModels lookup", async () => {
    const { client, mockListModels } = makeRuntime();

    await expect(resolveModelOfferingSelection(client, "")).rejects.toThrow(
      "offeringId must be an exact non-empty identifier",
    );
    expect(mockListModels).not.toHaveBeenCalled();
  });

  it("rejects when listModels fails", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockRejectedValue(new Error("Network error"));

    await expect(
      resolveModelOfferingSelection(client, "sonnet-4.6-adaptive"),
    ).rejects.toThrow("resolve model Offering failed: Network error");
  });

  it("rejects unauthenticated selection without querying the catalog", async () => {
    const { client, mockListModels } = makeRuntime({ accessToken: null });

    await expect(
      resolveModelOfferingSelection(client, "sonnet-4.6-adaptive"),
    ).rejects.toThrow("authenticated model access is required");
    expect(mockListModels).not.toHaveBeenCalled();
  });
});
