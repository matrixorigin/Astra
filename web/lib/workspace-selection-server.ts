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

function resolveLiveEdgeForSelection(
  edges: EdgeStatusEntry[],
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
) {
  return edges.find(
    (candidate) => candidate.edge_agent_id === selection.edgeAgentId,
  );
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
    const replacement = status.edges.find((candidate) =>
      edgeWorkspaceMatchesSelection(candidate, selection),
    );
    const providerLabel = selection.displayName ?? selection.edgeAgentId;
    const code = replacement
      ? "workspace_edge_stale_selection"
      : "workspace_edge_unavailable";
    const detail = replacement
      ? `Execution provider ${providerLabel} is no longer the active connection for ${selection.cwd}. Choose the currently connected file environment (${replacement.hostname ?? replacement.edge_agent_id}) and retry. No alternate execution provider is available for this file environment.`
      : `Execution provider ${providerLabel} is not connected or no longer registered. Reconnect it or choose an available file environment. No alternate execution provider is available for this file environment.`;
    throw new RuntimeClientError({
      operation: "verify edge workspace binding",
      path: PATH_EDGES_STATUS,
      status: 409,
      code,
      detail,
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
      code: "workspace_path_mismatch",
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
