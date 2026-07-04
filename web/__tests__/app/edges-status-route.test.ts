// @vitest-environment node

vi.mock("@/lib/runtime-client", () => ({
  getRuntimeClient: vi.fn(),
}));

import { getRuntimeClient } from "@/lib/runtime-client";
import { GET } from "@/app/api/edges/status/route";

const mockGetRuntimeClient = vi.mocked(getRuntimeClient);

describe("/api/edges/status route", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("returns an empty edge list when runtime is unavailable", async () => {
    mockGetRuntimeClient.mockResolvedValue(null);

    const response = await GET();

    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ edges: [] });
  });

  it("proxies runtime edge status when runtime is available", async () => {
    mockGetRuntimeClient.mockResolvedValue({
      get: vi.fn().mockResolvedValue({
        edges: [
          {
            edge_agent_id: "edge-1",
            hostname: "MacBook Pro",
            workspace_dir: "/Users/test/astra",
            connected_secs: 12,
          },
        ],
      }),
    } as never);

    const response = await GET();

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
