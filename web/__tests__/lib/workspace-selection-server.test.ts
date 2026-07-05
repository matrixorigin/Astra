// @vitest-environment node

import { verifyLiveWorkspaceSelection } from "@/lib/workspace-selection-server";

function runtimeWithEdges(edges: Array<Record<string, unknown>>) {
  return {
    get: vi.fn().mockResolvedValue({ edges }),
  };
}

describe("verifyLiveWorkspaceSelection", () => {
  it("rebinds a durable edge workspace selection to the live provider for the same cwd", async () => {
    const selection = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-old-random",
      displayName: "macpro.local",
      cwd: "/Users/test/astra",
    };

    await expect(
      verifyLiveWorkspaceSelection(
        selection,
        runtimeWithEdges([
          {
            edge_agent_id: "edge-new-stable",
            hostname: "macpro.local",
            workspace_dir: "/Users/test/astra",
            connected_secs: 2,
          },
        ]) as never,
      ),
    ).resolves.toEqual({
      kind: "edge_workspace",
      edgeAgentId: "edge-new-stable",
      displayName: "macpro.local",
      cwd: "/Users/test/astra",
    });
  });

  it("prefers an exact edge id match so a wrong cwd remains a hard error", async () => {
    const selection = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-1",
      displayName: "macpro.local",
      cwd: "/Users/test/astra",
    };

    await expect(
      verifyLiveWorkspaceSelection(
        selection,
        runtimeWithEdges([
          {
            edge_agent_id: "edge-1",
            hostname: "macpro.local",
            workspace_dir: "/Users/test/other",
            connected_secs: 2,
          },
        ]) as never,
      ),
    ).rejects.toMatchObject({ status: 409 });
  });
});
