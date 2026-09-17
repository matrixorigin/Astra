// @vitest-environment node

import { WebRuntimeClient } from "@/lib/runtime-client/server";
import {
  ModelOfferingSelectionError,
  requireSelectedOfferingId,
  resolveModelOfferingSelection,
} from "@/lib/api/web-store";

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

  it("validates syntax without treating the browsing catalog as admission authority", async () => {
    const { client, mockListModels } = makeRuntime();

    const result = await resolveModelOfferingSelection(client, "sonnet-4.6-adaptive");
    expect(result).toEqual({ offeringId: "sonnet-4.6-adaptive" });
    expect(mockListModels).not.toHaveBeenCalled();
  });

  it("passes unknown or inactive ids to Server admission for exact validation", async () => {
    const { client, mockListModels } = makeRuntime();
    await expect(resolveModelOfferingSelection(client, "unknown-offering"))
      .resolves.toEqual({ offeringId: "unknown-offering" });
    await expect(resolveModelOfferingSelection(client, "offer-disabled"))
      .resolves.toEqual({ offeringId: "offer-disabled" });
    expect(mockListModels).not.toHaveBeenCalled();
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
