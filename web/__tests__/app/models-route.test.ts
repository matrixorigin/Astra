// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeClient } from "@/lib/runtime-client";
import { GET } from "@/app/api/models/route";

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

describe("/api/models route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("does not return fallback models when Model Access fails", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient(
        "astra-access",
        vi.fn().mockRejectedValue(new Error("provider catalog down")),
      ) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(503);
    expect(body).toEqual({
      error: "model_access_unavailable",
      detail: "provider catalog down",
      action: "sign_in_or_retry",
    });
  });

  it("returns typed recovery when no Offering is eligible", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", vi.fn().mockResolvedValue({
        accesses: [{
          id: "self-hosted",
          kind: "self_hosted",
          label: "Self-hosted",
          execution_placement: "server",
          status: "unavailable",
          available_model_count: 0,
          actions: ["contact_administrator"],
        }],
        offerings: [],
        observed_at: "2026-07-20T00:00:00Z",
      })) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(200);
    expect(body).toEqual({
      items: [],
      accesses: [{
        id: "self-hosted",
        kind: "self_hosted",
        label: "Self-hosted",
        execution_placement: "server",
        status: "unavailable",
        available_model_count: 0,
        actions: ["contact_administrator"],
      }],
      observedAt: "2026-07-20T00:00:00Z",
      source: "astra",
      status: "unavailable",
      actions: ["contact_administrator"],
    });
  });

  it("does not advertise fake models when authentication is unavailable", async () => {
    mockRequireRuntimeClient.mockRejectedValue(new Error("sign in required"));

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(503);
    expect(body).toEqual({
      error: "model_access_unavailable",
      detail: "sign in required",
      action: "sign_in_or_retry",
    });
  });
});
