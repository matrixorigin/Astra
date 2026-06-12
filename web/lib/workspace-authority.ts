import type { WorkspaceSelection } from "@/lib/api/types";

export type WorkspaceAuthorityError = {
  code:
    | "workspace_required"
    | "workspace_local_code_on_server_sandbox"
    | "workspace_path_mismatch";
  message: string;
};

export function defaultWorkspaceBinding() {
  return {
    kind: "server_sandbox",
    display_name: "Server sandbox",
    authority: "read_write",
    fallback_policy: "disabled",
  };
}

export function defaultExecutorBinding() {
  return {
    kind: "server_local",
    executor_id: "server-local",
    display_name: "Server sandbox",
    transport: "server_local",
    status: "online",
  };
}

export function edgeWorkspaceBinding(
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
) {
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
) {
  return {
    kind: "edge_agent",
    executor_id: selection.edgeAgentId,
    display_name: selection.displayName ?? selection.edgeAgentId,
    transport: "edge_ws",
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

export function localCodeIntent(message: string) {
  const text = message.trim();

  // Explicit local tool commands that inherently operate on CWD
  if (
    /\b(?:git\s+(?:status|diff|show|log|branch|checkout|merge|rebase|commit)|(?:npm|pnpm|yarn)\s+(?:test|run|install|build|lint)|cargo\s+(?:test|build|check|clippy|fmt)|pytest|go\s+test|make(?:\s+\w[\w:-]*)?)\b/i.test(
      text,
    )
  ) {
    return true;
  }

  // Explicit local path mentions
  if (extractLocalPathMentions(text).length > 0) {
    return true;
  }

  // "this/current" workspace context — unambiguously local
  if (
    /\b(?:this|current)\s+(?:repo|repository|project|workspace|directory|folder)\b/i.test(
      text,
    ) ||
    /\b(?:review|inspect|fix|modify|edit|run|test|build|open|switch(?:\s+to)?|cd)\s+(?:this|current)\b/i.test(
      text,
    )
  ) {
    return true;
  }

  return false;
}

export function extractLocalPathMentions(message: string) {
  const matches = message.match(
    /(?:~\/|\$HOME\/|\$\{HOME\}\/|\/Users\/|\/home\/|\/Volumes\/|[A-Za-z]:\\)[^\s`'")\]}，。；、！？]+/g,
  );
  return (matches ?? []).map((match) =>
    match.replace(/[.,;:!?，。；：！？、]+$/u, ""),
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

export function edgeWorkspaceOwnsPath(
  selection: WorkspaceSelection,
  rawPath: string,
) {
  if (selection.kind !== "edge_workspace") {
    return false;
  }
  const cwd = normalizeSlashPath(selection.cwd);
  let path = rawPath;
  if (
    rawPath.startsWith("~/") ||
    rawPath.startsWith("$HOME/") ||
    rawPath.startsWith("${HOME}/")
  ) {
    const home = cwd.match(/^\/Users\/[^/]+|^\/home\/[^/]+/)?.[0];
    if (!home) {
      return false;
    }
    const suffix = rawPath.startsWith("~/")
      ? rawPath.slice(2)
      : rawPath.startsWith("$HOME/")
        ? rawPath.slice(6)
        : rawPath.slice(8);
    path = `${home}/${suffix}`;
  }
  const normalizedPath = normalizeSlashPath(path);
  return normalizedPath === cwd || normalizedPath.startsWith(`${cwd}/`);
}

export function validateWorkspaceAuthority(
  message: string,
  selection: WorkspaceSelection | null | undefined,
): WorkspaceAuthorityError | null {
  if (!localCodeIntent(message)) {
    return null;
  }
  if (!selection) {
    return {
      code: "workspace_required",
      message:
        "Select a connected edge workspace in the Workspace bar, then retry this local-code request.",
    };
  }
  if (selection.kind === "server_sandbox") {
    return {
      code: "workspace_local_code_on_server_sandbox",
      message:
        "This prompt refers to local code, but Server sandbox cannot access your local paths. Select a connected edge workspace in the Workspace bar, then retry.",
    };
  }
  const pathMentions = extractLocalPathMentions(message);
  const foreignPath = pathMentions.find(
    (path) => !edgeWorkspaceOwnsPath(selection, path),
  );
  if (foreignPath) {
    return {
      code: "workspace_path_mismatch",
      message: `The selected edge workspace is ${selection.cwd}, but the prompt references ${foreignPath}. Select an edge workspace that owns that path, then retry.`,
    };
  }
  return null;
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
  return {
    workspaceBinding: defaultWorkspaceBinding(),
    executorBinding: defaultExecutorBinding(),
    edgeProfile: undefined,
  };
}
