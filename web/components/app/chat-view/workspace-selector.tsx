"use client";

import {
  AlertTriangle,
  HardDrive,
  MessageSquare,
  Monitor,
  RefreshCw,
} from "lucide-react";
import type { EdgeStatusResponse, WorkspaceSelection } from "@/lib/api/types";

export function WorkspaceSelector({
  selection,
  explicit,
  edges,
  loading,
  error,
  disabled,
  onRefresh,
  onSelect,
}: {
  selection: WorkspaceSelection | null;
  explicit: boolean;
  edges: EdgeStatusResponse["edges"];
  loading: boolean;
  error: string | null;
  disabled: boolean;
  onRefresh: () => void | Promise<void>;
  onSelect: (selection: WorkspaceSelection) => void;
}) {
  const edgeOptions = edges
    .filter((edge) => edge.workspace_dir?.trim())
    .map((edge) => ({
      edgeAgentId: edge.edge_agent_id,
      displayName: edge.hostname ?? edge.edge_agent_id,
      cwd: edge.workspace_dir!,
      connectedSecs: edge.connected_secs,
    }));
  const selectedEdge =
    selection?.kind === "edge_workspace"
      ? edgeOptions.find(
          (edge) =>
            edge.edgeAgentId === selection.edgeAgentId &&
            edge.cwd === selection.cwd,
        )
      : null;
  const selectedEdgeMissing =
    explicit && selection?.kind === "edge_workspace" && !selectedEdge;
  const selectedOfflineLabel =
    selection?.kind === "edge_workspace"
      ? `${selection.displayName ?? selection.edgeAgentId} · ${selection.cwd}`
      : "";

  return (
    <div className="mb-2 flex flex-wrap items-center gap-2 rounded-[14px] border border-border/70 bg-surface/95 px-3 py-2 text-xs text-text-muted shadow-[0_0.15rem_0.8rem_rgba(28,25,23,0.05)]">
      <span className="font-medium text-text">Workspace</span>
      {!explicit || !selection ? (
        <span className="inline-flex min-w-0 items-center gap-1.5 rounded-full bg-bg px-2.5 py-1.5 font-medium text-text-secondary">
          <MessageSquare className="size-3.5 shrink-0 text-text-muted" />
          <span className="truncate">Chat only</span>
          <span className="hidden border-l border-border pl-1.5 font-normal text-text-muted sm:inline">
            No code workspace
          </span>
        </span>
      ) : null}
      <button
        type="button"
        disabled={disabled}
        onClick={() => onSelect({ kind: "server_sandbox" })}
        className={[
          "inline-flex max-w-full items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
          explicit && selection?.kind === "server_sandbox"
            ? "bg-text text-white"
            : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
        ].join(" ")}
      >
        <HardDrive className="size-3.5 shrink-0" />
        <span className="truncate">Server sandbox</span>
      </button>
      {edgeOptions.map((edge) => {
        const selected =
          explicit &&
          selection?.kind === "edge_workspace" &&
          selection.edgeAgentId === edge.edgeAgentId &&
          selection.cwd === edge.cwd;
        return (
          <button
            key={`${edge.edgeAgentId}:${edge.cwd}`}
            type="button"
            disabled={disabled}
            title={`${edge.displayName} · ${edge.cwd}`}
            onClick={() =>
              onSelect({
                kind: "edge_workspace",
                edgeAgentId: edge.edgeAgentId,
                displayName: edge.displayName,
                cwd: edge.cwd,
              })
            }
            className={[
              "inline-flex min-w-0 max-w-[min(30rem,100%)] items-center gap-1.5 rounded-full px-2.5 py-1.5 font-medium transition focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-not-allowed disabled:opacity-50",
              selected
                ? "bg-text text-white"
                : "bg-bg text-text-secondary hover:bg-surface-muted hover:text-text",
            ].join(" ")}
          >
            <Monitor className="size-3.5 shrink-0" />
            <span className="truncate">{edge.displayName}</span>
            <span
              className={[
                "min-w-0 truncate border-l pl-1.5 font-normal",
                selected
                  ? "border-white/30 text-white/75"
                  : "border-border text-text-muted",
              ].join(" ")}
            >
              {edge.cwd}
            </span>
            <span className="sr-only">
              connected for {edge.connectedSecs} seconds
            </span>
          </button>
        );
      })}
      <button
        type="button"
        disabled={loading}
        onClick={() => {
          void onRefresh();
        }}
        className="inline-flex size-7 items-center justify-center rounded-full bg-bg text-text-muted transition hover:bg-surface-muted hover:text-text focus:outline-none focus:ring-2 focus:ring-accent/30 disabled:cursor-wait disabled:opacity-60"
        aria-label="Refresh edge workspaces"
        title="Refresh edge workspaces"
      >
        <RefreshCw
          className={["size-3.5", loading ? "animate-spin" : ""].join(" ")}
        />
      </button>
      {edgeOptions.length === 0 && !loading ? (
        <span className="text-text-muted">No edge workspaces online</span>
      ) : null}
      {selectedEdgeMissing ? (
        <div
          className="flex min-w-0 max-w-full flex-wrap items-center gap-2 rounded-[10px] border border-warning/30 bg-warning/10 px-2.5 py-1.5 text-warning"
          role="status"
          aria-label={`Selected edge workspace is offline: ${selectedOfflineLabel}`}
        >
          <AlertTriangle className="size-3.5 shrink-0" />
          <span className="min-w-0 max-w-[min(28rem,100%)] truncate">
            Edge offline · {selectedOfflineLabel}
          </span>
          <button
            type="button"
            disabled={disabled}
            onClick={() => onSelect({ kind: "server_sandbox" })}
            className="rounded-full bg-bg px-2 py-0.5 font-medium text-text transition hover:bg-surface-muted disabled:cursor-not-allowed disabled:opacity-50"
          >
            Use server sandbox
          </button>
        </div>
      ) : null}
      {error ? (
        <span className="max-w-full truncate text-warning">{error}</span>
      ) : null}
    </div>
  );
}
