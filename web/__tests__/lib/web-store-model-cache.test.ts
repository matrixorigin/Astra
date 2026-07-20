// @vitest-environment node

import { WebRuntimeClient } from "@/lib/runtime-client/server";
import {
  ModelOfferingSelectionError,
  requireSelectedOfferingId,
  resolveModelOfferingSelection,
} from "@/lib/api/web-store";
import { resetModelCacheForTests } from "@/lib/api/model-cache";

vi.mock("@/lib/runtime-client/server", async (importOriginal) => {
  const actual =
    await importOriginal<typeof import("@/lib/runtime-client/server")>();
  return { ...actual, WebRuntimeClient: vi.fn() };
});

const MockedWRC = WebRuntimeClient as ReturnType<typeof vi.fn>;

let tokenCounter = 0;

function offering(offeringId: string, name: string) {
  return {
    offering_id: offeringId,
    access_id: "self-hosted",
    access_kind: "self_hosted",
    access_label: "Self-hosted",
    execution_placement: "server",
    name,
    provider: "openai",
    description: null,
    is_active: true,
    context_window: 128_000,
    max_completion_tokens: null,
    architecture: null,
    thinking_capability: null,
  };
}

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

  it("requires an explicit Offering instead of inventing a default", () => {
    let failure: unknown;
    try {
      requireSelectedOfferingId(null);
    } catch (error) {
      failure = error;
    }
    expect(failure).toBeInstanceOf(ModelOfferingSelectionError);
    expect(failure).toMatchObject({ code: "invalid_selection" });
  });

  it("admits an exact active Offering id on first call", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      offering("sonnet-4.6-adaptive", "Sonnet 4.6"),
    ]);

    const result = await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    expect(result).toEqual({ offeringId: "sonnet-4.6-adaptive" });
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("caches listModels — second call does not call listModels again", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      offering("sonnet-4.6-adaptive", "Sonnet 4.6"),
    ]);

    await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    // With cache: only 1 call
    expect(mockListModels).toHaveBeenCalledTimes(1);
  });

  it("rejects a forged or stale Offering instead of falling back to a model name", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([offering("opus-4.7", "Opus 4.7")]);

    await expect(
      resolveModelOfferingSelection(client, "unknown-model-xyz"),
    ).rejects.toMatchObject({
      code: "offering_unavailable",
    });
  });

  it("rejects an exact Offering after it becomes inactive", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockResolvedValue([
      { ...offering("offer-disabled", "Disabled"), is_active: false },
    ]);

    await expect(
      resolveModelOfferingSelection(client, "offer-disabled"),
    ).rejects.toMatchObject({
      code: "offering_unavailable",
    });
  });

  it("rejects missing model before listModels lookup", async () => {
    const { client, mockListModels } = makeRuntime();

    await expect(
      resolveModelOfferingSelection(client, ""),
    ).rejects.toMatchObject({
      code: "invalid_selection",
    });
    expect(mockListModels).not.toHaveBeenCalled();
  });

  it("rejects when listModels fails", async () => {
    const { client, mockListModels } = makeRuntime();
    mockListModels.mockRejectedValue(new Error("Network error"));

    await expect(
      resolveModelOfferingSelection(client, "sonnet-4.6-adaptive"),
    ).rejects.toMatchObject({
      code: "catalog_unavailable",
    });
  });

  it("rejects unauthenticated selection without querying the catalog", async () => {
    const { client, mockListModels } = makeRuntime({ accessToken: null });

    await expect(
      resolveModelOfferingSelection(client, "sonnet-4.6-adaptive"),
    ).rejects.toMatchObject({
      code: "authentication_required",
    });
    expect(mockListModels).not.toHaveBeenCalled();
  });
});
