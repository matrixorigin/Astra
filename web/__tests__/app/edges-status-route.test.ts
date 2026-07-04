// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  requireRuntimeClient: vi.fn(),
}));

import { requireRuntimeClient } from "@/lib/runtime-client";
import { GET } from "@/app/api/edges/status/route";

const mockRequireRuntimeClient = vi.mocked(requireRuntimeClient);

describe("/api/edges/status route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("returns an empty edge list when runtime is unavailable", async () => {
    mockRequireRuntimeClient.mockRejectedValue({
      status: 503,
      detail: "Runtime unavailable.",
    });

    const response = await GET();

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ edges: [] });
  });

  it("requires runtime auth before probing edge status", async () => {
    mockRequireRuntimeClient.mockRejectedValue({
      status: 401,
      detail: "Runtime authentication is missing.",
    });

    const response = await GET();

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      error: "Runtime authentication is missing.",
    });
  });

  it("proxies runtime edge status when runtime is available", async () => {
    const get = vi.fn().mockResolvedValue({
      edges: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 12,
        },
      ],
    });
    mockRequireRuntimeClient.mockResolvedValue({ get } as never);

    const response = await GET();

    expect(mockRequireRuntimeClient).toHaveBeenCalledWith({
      auth: "required",
      operation: "list edge executors",
    });
    expect(get).toHaveBeenCalledWith(
      expect.any(String),
      expect.objectContaining({
        auth: "required",
        operation: "list edge executors",
      }),
    );
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      edges: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 12,
        },
      ],
    });
  });
});
