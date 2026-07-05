import { PATH_EDGES_STATUS } from "@astra/sdk";
import type { EdgeStatusResponse, WorkspaceSelection } from "@/lib/api/types";
import {
  RuntimeClientError,
  type WebRuntimeClient,
} from "@/lib/runtime-client";
import { normalizeSlashPath } from "@/lib/workspace-authority";

type EdgeStatusEntry = EdgeStatusResponse["edges"][number];

function edgeWorkspaceMatchesSelection(
  edge: EdgeStatusEntry,
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
) {
  const liveCwd = edge.workspace_dir?.trim() ?? "";
  return (
    liveCwd &&
    normalizeSlashPath(liveCwd) === normalizeSlashPath(selection.cwd)
  );
}

function edgeDisplayMatchesSelection(
  edge: EdgeStatusEntry,
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
) {
  const displayName = selection.displayName?.trim();
  if (!displayName) {
    return true;
  }
  return edge.hostname?.trim() === displayName;
}

function newestConnectedFirst(left: EdgeStatusEntry, right: EdgeStatusEntry) {
  return left.connected_secs - right.connected_secs;
}

function resolveLiveEdgeForSelection(
  edges: EdgeStatusEntry[],
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
) {
  const exact = edges.find(
    (candidate) => candidate.edge_agent_id === selection.edgeAgentId,
  );
  if (exact) {
    return exact;
  }

  const sameWorkspace = edges
    .filter((candidate) => edgeWorkspaceMatchesSelection(candidate, selection))
    .sort(newestConnectedFirst);
  const sameDisplay = sameWorkspace.filter((candidate) =>
    edgeDisplayMatchesSelection(candidate, selection),
  );
  if (selection.displayName?.trim()) {
    return sameDisplay[0];
  }
  return sameWorkspace[0];
}

export async function verifyLiveWorkspaceSelection(
  selection: WorkspaceSelection | null | undefined,
  runtime: WebRuntimeClient,
): Promise<WorkspaceSelection | null | undefined> {
  if (selection?.kind !== "edge_workspace") {
    return selection;
  }

  const status = await runtime.get<EdgeStatusResponse>(PATH_EDGES_STATUS, {
    auth: "required",
    operation: "verify edge workspace binding",
  });
  const edge = resolveLiveEdgeForSelection(status.edges, selection);
  if (!edge) {
    throw new RuntimeClientError({
      operation: "verify edge workspace binding",
      path: PATH_EDGES_STATUS,
      status: 409,
      detail: `Execution provider ${selection.displayName ?? selection.edgeAgentId} is offline. Reconnect it or choose an available file environment. No alternate execution provider is available for this file environment.`,
    });
  }

  const liveCwd = edge.workspace_dir?.trim() ?? "";
  if (
    !liveCwd ||
    normalizeSlashPath(liveCwd) !== normalizeSlashPath(selection.cwd)
  ) {
    const current = liveCwd
      ? `currently reports ${liveCwd}`
      : "does not report a workspace";
    throw new RuntimeClientError({
      operation: "verify edge workspace binding",
      path: PATH_EDGES_STATUS,
      status: 409,
      detail: `Execution provider ${edge.hostname ?? selection.displayName ?? selection.edgeAgentId} ${current}, not ${selection.cwd}. Choose the file environment that owns that path, then retry. No alternate execution provider is available for this file environment.`,
    });
  }

  return {
    ...selection,
    edgeAgentId: edge.edge_agent_id,
    displayName:
      edge.hostname ?? selection.displayName ?? selection.edgeAgentId,
    cwd: liveCwd,
  };
}
