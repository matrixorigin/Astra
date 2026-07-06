// @vitest-environment node

import { verifyLiveWorkspaceSelection } from "@/lib/workspace-selection-server";

function runtimeWithEdges(edges: Array<Record<string, unknown>>) {
  return {
    get: vi.fn().mockResolvedValue({ edges }),
  };
}

describe("verifyLiveWorkspaceSelection", () => {
  it("rejects a stale edge provider id even when cwd and hostname match", async () => {
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
    ).rejects.toMatchObject({
      status: 409,
      code: "workspace_edge_stale_selection",
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
    ).rejects.toMatchObject({
      status: 409,
      code: "workspace_path_mismatch",
    });
  });

  it("distinguishes an unavailable edge from a stale selected provider id", async () => {
    const selection = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-offline",
      displayName: "macpro.local",
      cwd: "/Users/test/astra",
    };

    await expect(
      verifyLiveWorkspaceSelection(selection, runtimeWithEdges([]) as never),
    ).rejects.toMatchObject({
      status: 409,
      code: "workspace_edge_unavailable",
    });
  });

  it("canonicalizes cwd and display name only after the provider id matches", async () => {
    const selection = {
      kind: "edge_workspace" as const,
      edgeAgentId: "edge-1",
      displayName: "old-name",
      cwd: "/Users/test/astra",
    };

    await expect(
      verifyLiveWorkspaceSelection(
        selection,
        runtimeWithEdges([
          {
            edge_agent_id: "edge-1",
            hostname: "macpro.local",
            workspace_dir: "/Users/test//astra",
            connected_secs: 2,
          },
        ]) as never,
      ),
    ).resolves.toEqual({
      kind: "edge_workspace",
      edgeAgentId: "edge-1",
      displayName: "macpro.local",
      cwd: "/Users/test//astra",
    });
  });
});
