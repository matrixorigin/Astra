import type { WorkspaceBinding, ExecutorBinding } from "@astra/sdk";
import type { WorkspaceSelection } from "@/lib/api/types";

export function defaultWorkspaceBinding(): WorkspaceBinding {
  return {
    kind: "none",
    display_name: "Web",
    authority: "none",
    fallback_policy: "disabled",
  };
}

export function defaultExecutorBinding(): ExecutorBinding {
  return {
    kind: "server_local",
    executor_id: "server-control-plane",
    display_name: "Server control plane",
    transport: "server_local",
    status: "online",
  };
}

export function edgeWorkspaceBinding(
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
): WorkspaceBinding {
  return {
    kind: "edge_workspace",
    display_name: selection.displayName ?? selection.edgeAgentId,
    cwd: selection.cwd,
    authority: "read_write",
    fallback_policy: "disabled",
  };
}

export function edgeExecutorBinding(
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
): ExecutorBinding {
  return {
    kind: "edge_agent",
    executor_id: selection.edgeAgentId,
    display_name: selection.displayName ?? selection.edgeAgentId,
    transport: "edge_ws",
    status: "online",
  };
}

export function serverSandboxWorkspaceBinding(): WorkspaceBinding {
  return {
    kind: "server_sandbox",
    display_name: "Server sandbox",
    authority: "read_write",
    fallback_policy: "disabled",
  };
}

export function serverSandboxExecutorBinding(): ExecutorBinding {
  return {
    kind: "server_local",
    executor_id: "server-local",
    display_name: "Server sandbox",
    transport: "server_local",
    status: "online",
  };
}

export function normalizeWorkspaceSelection(
  value: unknown,
): WorkspaceSelection | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }
  const raw = value as Record<string, unknown>;
  if (raw.kind === "server_sandbox") {
    return { kind: "server_sandbox" };
  }
  if (raw.kind !== "edge_workspace") {
    return undefined;
  }
  const edgeAgentId =
    typeof raw.edgeAgentId === "string"
      ? raw.edgeAgentId.trim()
      : typeof raw.edge_agent_id === "string"
        ? raw.edge_agent_id.trim()
        : "";
  const cwd = typeof raw.cwd === "string" ? raw.cwd.trim() : "";
  if (!edgeAgentId || !cwd) {
    return undefined;
  }
  const displayName =
    typeof raw.displayName === "string"
      ? raw.displayName.trim()
      : typeof raw.display_name === "string"
        ? raw.display_name.trim()
        : "";
  return {
    kind: "edge_workspace",
    edgeAgentId,
    displayName: displayName || null,
    cwd,
  };
}

export function sameWorkspaceSelection(
  left: WorkspaceSelection | null | undefined,
  right: WorkspaceSelection | null | undefined,
) {
  if (!left && !right) {
    return true;
  }
  if (!left || !right || left.kind !== right.kind) {
    return false;
  }
  if (left.kind === "server_sandbox") {
    return true;
  }
  return (
    right.kind === "edge_workspace" &&
    left.edgeAgentId === right.edgeAgentId &&
    left.cwd === right.cwd &&
    (left.displayName ?? null) === (right.displayName ?? null)
  );
}

export function normalizeSlashPath(path: string) {
  const absolute = path.replace(/\\/g, "/").replace(/\/+/g, "/");
  const parts: string[] = [];
  for (const part of absolute.split("/")) {
    if (!part || part === ".") {
      continue;
    }
    if (part === "..") {
      parts.pop();
      continue;
    }
    parts.push(part);
  }
  return absolute.startsWith("/") ? `/${parts.join("/")}` : parts.join("/");
}

export function resolveWorkspaceBindings(selection: WorkspaceSelection | null) {
  if (selection?.kind === "edge_workspace") {
    return {
      workspaceBinding: edgeWorkspaceBinding(selection),
      executorBinding: edgeExecutorBinding(selection),
      edgeProfile: {
        cwd: selection.cwd,
        edge_agent_id: selection.edgeAgentId,
      },
    };
  }
  if (selection?.kind === "server_sandbox") {
    return {
      workspaceBinding: serverSandboxWorkspaceBinding(),
      executorBinding: serverSandboxExecutorBinding(),
      edgeProfile: undefined,
    };
  }
  return {
    workspaceBinding: defaultWorkspaceBinding(),
    executorBinding: defaultExecutorBinding(),
    edgeProfile: undefined,
  };
}
