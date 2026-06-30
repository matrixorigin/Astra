// @vitest-environment node

vi.mock("@/lib/api/web-store", () => ({
  listModelSummaries: vi.fn(() => [
    {
      id: "static-model",
      name: "Static Model",
      subtitle: "Static fallback",
      tier: "included",
    },
  ]),
}));

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { listModelSummaries } from "@/lib/api/web-store";
import { requireRuntimeClient } from "@/lib/runtime-client";
import { GET } from "@/app/api/models/route";

const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);
const mockListModelSummaries = vi.mocked(listModelSummaries);

function runtimeClient(
  accessToken: string | undefined,
  listModels: ReturnType<typeof vi.fn>,
) {
  return {
    config: { accessToken },
    sdk: { listModels },
  };
}

describe("/api/models route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("does not return fallback models when authenticated listModels fails", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient(
        "astra-access",
        vi.fn().mockRejectedValue(new Error("provider catalog down")),
      ) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(502);
    expect(body).toEqual({
      error: "runtime_models_unavailable",
      detail: "provider catalog down",
    });
    expect(mockListModelSummaries).not.toHaveBeenCalled();
  });

  it("does not return fallback models when authenticated listModels is empty", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", vi.fn().mockResolvedValue([])) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(502);
    expect(body).toEqual({
      error: "runtime_models_unavailable",
      detail: "Runtime returned no active models for the authenticated user.",
    });
    expect(mockListModelSummaries).not.toHaveBeenCalled();
  });

  it("keeps static models for unauthenticated listModels failures", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient(
        undefined,
        vi.fn().mockRejectedValue(new Error("runtime unavailable")),
      ) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(200);
    expect(body).toEqual({
      items: [
        {
          id: "static-model",
          name: "Static Model",
          subtitle: "Static fallback",
          tier: "included",
        },
      ],
      source: "fallback",
    });
    expect(mockListModelSummaries).toHaveBeenCalledTimes(1);
  });
});
