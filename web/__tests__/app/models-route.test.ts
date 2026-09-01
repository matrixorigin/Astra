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
          status: "action_required",
          reason: "no_eligible_offerings",
          usable: false,
          retry_after_seconds: null,
          available_model_count: 0,
          actions: ["contact_administrator"],
        }],
        offerings: [],
        default_offering_id: null,
        default_resolution: { state: "missing" },
        next_cursor: null,
        limit: 200,
        total: 0,
        catalog_revision: "sha256:empty",
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
        status: "action_required",
        reason: "no_eligible_offerings",
        usable: false,
        retry_after_seconds: null,
        available_model_count: 0,
        actions: ["contact_administrator"],
      }],
      defaultOfferingId: null,
      defaultResolution: { state: "missing" },
      catalogRevision: "sha256:empty",
      observedAt: "2026-07-20T00:00:00Z",
      source: "astra",
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

  it("keeps valid Offerings visible when the provider default is invalid", async () => {
    mockRequireRuntimeClient.mockResolvedValue(
      runtimeClient("astra-access", vi.fn().mockResolvedValue({
        accesses: [{
          id: "this-device",
          kind: "this_device",
          label: "This device",
          execution_placement: "edge",
          status: "ready",
          reason: null,
          usable: true,
          retry_after_seconds: null,
          available_model_count: 1,
          actions: [],
        }],
        offerings: [{
          offering_id: "offer-valid",
          access_id: "this-device",
          access_kind: "this_device",
          access_label: "This device",
          execution_placement: "edge",
          name: "valid-model",
          provider: "moi",
          description: null,
          architecture: null,
          context_window: 8192,
          max_completion_tokens: null,
          thinking_capability: null,
          is_active: true,
        }],
        default_offering_id: null,
        default_resolution: {
          state: "invalid",
          reason: "not_effective_offering",
        },
        catalog_revision: "sha256:invalid-provider-default",
        observed_at: "2026-08-10T00:00:00Z",
      })) as never,
    );

    const response = await GET();
    const body = await response.json();

    expect(response.status).toBe(200);
    expect(body.items).toEqual([expect.objectContaining({ id: "offer-valid" })]);
    expect(body.defaultOfferingId).toBeNull();
    expect(body.defaultResolution).toMatchObject({
      state: "invalid",
      reason: "not_effective_offering",
    });
  });
});
