// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeClient } from "@/lib/runtime-client";
import { GET } from "@/app/api/models/route";

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

    expect(response.status).toBe(503);
    expect(body).toEqual({
      error: "model_access_unavailable",
      detail: "provider catalog down",
      action: "sign_in_or_retry",
    });
  });

  it("does not return fallback models when authenticated listModels is empty", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", vi.fn().mockResolvedValue([])) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(200);
    expect(body).toEqual({
      items: [],
      source: "astra",
      status: "unavailable",
      action: "contact_admin",
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
