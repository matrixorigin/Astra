// @vitest-environment node

import { WebRuntimeClient } from "@/lib/runtime-client/server";
import { requireKnownBackendModelName } from "@/lib/api/web-store";
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

describe("requireKnownBackendModelName", () => {
  beforeEach(() => {
    resetModelCacheForTests();
  });

  it("accepts an exact canonical model name from listModels", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "model-row-1", name: "sonnet-4.6-adaptive" },
    ]);

    const result = await requireKnownBackendModelName(client, "sonnet-4.6-adaptive");
    expect(result).toBe("sonnet-4.6-adaptive");
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("trims the requested model before validating it", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "model-row-1", name: "sonnet-4.6-adaptive" },
    ]);

    const result = await requireKnownBackendModelName(
      client,
      "  sonnet-4.6-adaptive  ",
    );
    expect(result).toBe("sonnet-4.6-adaptive");
  });

  it("rejects database model_id values instead of mapping them", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "model-row-1", name: "sonnet-4.6-adaptive" },
    ]);

    await expect(
      requireKnownBackendModelName(client, "model-row-1"),
    ).rejects.toThrow('Unknown model "model-row-1"');
  });

  it("caches listModels for repeated exact canonical names", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "model-row-1", name: "sonnet-4.6-adaptive" },
    ]);

    await requireKnownBackendModelName(client, "sonnet-4.6-adaptive");
    await requireKnownBackendModelName(client, "sonnet-4.6-adaptive");
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("rejects unknown model strings instead of forwarding them", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { model_id: "model-row-1", name: "opus-4.7" },
    ]);

    await expect(
      requireKnownBackendModelName(client, "unknown-model-xyz"),
    ).rejects.toThrow('Unknown model "unknown-model-xyz"');
  });

  it("rejects a DeepSeek Pro alias instead of choosing the first DeepSeek model", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      {
        model_id: "be784f7f-b188-4f53-b7eb-997a9f95f66c",
        name: "deepseek-v4-flash-anthropic",
      },
      {
        model_id: "0ca5bdfb-471f-4776-b2d8-2d4bedf1e50a",
        name: "deepseek-v4-pro-official",
      },
    ]);

    await expect(
      requireKnownBackendModelName(client, "deepseek-v4-pro"),
    ).rejects.toThrow('Unknown model "deepseek-v4-pro"');
  });

  it("rejects missing model before listModels lookup", async () => {
    const { client, mockListModels } = makeRuntime();

    await expect(requireKnownBackendModelName(client, "")).rejects.toThrow(
      "model is required",
    );
    expect(mockListModels).not.toHaveBeenCalled();
  });

  it("rejects and invalidates the cache on listModels errors", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels
      .mockRejectedValueOnce(new Error("Network error"))
      .mockResolvedValueOnce([
        { model_id: "model-row-1", name: "sonnet-4.6-adaptive" },
      ]);

    await expect(
      requireKnownBackendModelName(client, "sonnet-4.6-adaptive"),
    ).rejects.toThrow("resolve runtime model: Network error");
    const result = await requireKnownBackendModelName(
      client,
      "sonnet-4.6-adaptive",
    );
    expect(result).toBe("sonnet-4.6-adaptive");
    expect(mockListModels).toHaveBeenCalledTimes(2);
  });

  it("rejects before listModels when runtime auth is missing", async () => {
    const { client, mockListModels } = makeRuntime({ accessToken: null });

    await expect(
      requireKnownBackendModelName(client, "sonnet-4.6-adaptive"),
    ).rejects.toThrow("Runtime authentication is missing.");
    expect(mockListModels).not.toHaveBeenCalled();
  });
});
