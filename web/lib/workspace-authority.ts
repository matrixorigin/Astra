import type { WorkspaceBinding, ExecutorBinding } from "@astra/sdk";
import type { WorkspaceAuthority, WorkspaceSelection } from "@/lib/api/types";

type WorkspaceAuthorityError = {
  code: string;
  error: string;
  status: number;
};

const DEFAULT_SELECTED_WORKSPACE_AUTHORITY: WorkspaceAuthority = "read_write";
const LOCAL_PATH_ROOTS = [
  "/Users/",
  "/home/",
  "/workspace/",
  "/workspaces/",
  "/tmp/",
  "/private/tmp/",
  "/var/",
  "/etc/",
  "/opt/",
  "/mnt/",
  "/Volumes/",
];
const CASE_INSENSITIVE_LOCAL_ROOTS = [
  "/Users/",
  "/Volumes/",
  "/private/tmp/",
  "/private/var/",
];

function workspaceAuthority(value: unknown): WorkspaceAuthority | undefined {
  return value === "read_only" || value === "read_write" ? value : undefined;
}

function selectedWorkspaceAuthority(selection: WorkspaceSelection) {
  return selection.authority ?? DEFAULT_SELECTED_WORKSPACE_AUTHORITY;
}

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
    authority: selectedWorkspaceAuthority(selection),
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

export function serverSandboxWorkspaceBinding(
  selection?: Extract<WorkspaceSelection, { kind: "server_sandbox" }>,
): WorkspaceBinding {
  return {
    kind: "server_sandbox",
    display_name: "Server sandbox",
    authority: selection ? selectedWorkspaceAuthority(selection) : "read_write",
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
  const authority = workspaceAuthority(raw.authority);
  if (raw.kind === "server_sandbox") {
    return {
      kind: "server_sandbox",
      ...(authority ? { authority } : {}),
    };
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
    ...(authority ? { authority } : {}),
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
    return selectedWorkspaceAuthority(left) === selectedWorkspaceAuthority(right);
  }
  return (
    right.kind === "edge_workspace" &&
    left.edgeAgentId === right.edgeAgentId &&
    left.cwd === right.cwd &&
    (left.displayName ?? null) === (right.displayName ?? null) &&
    selectedWorkspaceAuthority(left) === selectedWorkspaceAuthority(right)
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

function isLocalAbsolutePath(path: string) {
  const normalized = path.replace(/\\/g, "/");
  const lowered = normalized.toLowerCase();
  return (
    lowered.startsWith("~/") ||
    /^[a-z]:\//.test(lowered) ||
    LOCAL_PATH_ROOTS.some((root) => lowered.startsWith(root.toLowerCase()))
  );
}

function stripPathPunctuation(path: string) {
  return path.replace(/[.,;:!?)}\]]+$/g, "");
}

export function extractLocalPathMentions(content: string): string[] {
  const mentions = new Set<string>();
  const pathPattern =
    /(^|[\s"'`([{])((?:~\/|[A-Za-z]:[\\/]|\/)[^\s"'`<>]*)/g;
  let match: RegExpExecArray | null;
  while ((match = pathPattern.exec(content)) !== null) {
    const candidate = stripPathPunctuation(match[2] ?? "");
    if (candidate && isLocalAbsolutePath(candidate)) {
      mentions.add(candidate);
    }
  }
  return [...mentions];
}

function homePrefixFromWorkspace(cwd: string) {
  const normalized = normalizeSlashPath(cwd);
  const unixHome = normalized.match(/^\/(?:Users|home)\/[^/]+/);
  if (unixHome) {
    return unixHome[0];
  }
  const windowsHome = normalized.match(/^[A-Za-z]:\/Users\/[^/]+/);
  return windowsHome?.[0] ?? null;
}

function expandWorkspaceRelativeHome(path: string, cwd: string) {
  if (!path.startsWith("~/")) {
    return path;
  }
  const home = homePrefixFromWorkspace(cwd);
  return home ? `${home}/${path.slice(2)}` : path;
}

function pathUsesCaseInsensitiveRoot(path: string) {
  const normalized = normalizeSlashPath(path).toLowerCase();
  return (
    /^[a-z]:\//.test(normalized) ||
    CASE_INSENSITIVE_LOCAL_ROOTS.some((root) =>
      normalized.startsWith(root.toLowerCase()),
    )
  );
}

function ownedPathKey(path: string, caseInsensitive: boolean) {
  const normalized = normalizeSlashPath(path);
  return caseInsensitive ? normalized.toLowerCase() : normalized;
}

function edgeWorkspaceOwnsPath(
  selection: Extract<WorkspaceSelection, { kind: "edge_workspace" }>,
  path: string,
) {
  const workspaceRoot = normalizeSlashPath(selection.cwd);
  const expanded = expandWorkspaceRelativeHome(path, workspaceRoot);
  const normalizedPath = normalizeSlashPath(expanded);
  if (!workspaceRoot || !normalizedPath) {
    return false;
  }
  const caseInsensitive =
    pathUsesCaseInsensitiveRoot(workspaceRoot) ||
    pathUsesCaseInsensitiveRoot(normalizedPath);
  const workspaceKey = ownedPathKey(workspaceRoot, caseInsensitive);
  const pathKey = ownedPathKey(normalizedPath, caseInsensitive);
  return (
    pathKey === workspaceKey ||
    pathKey.startsWith(`${workspaceKey}/`)
  );
}

export function validateWorkspaceAuthority(
  content: string,
  selection: WorkspaceSelection | null | undefined,
): WorkspaceAuthorityError | null {
  const paths = extractLocalPathMentions(content);
  if (paths.length === 0) {
    return null;
  }

  if (!selection) {
    return {
      code: "workspace_required",
      error: `The referenced path requires a file environment: ${paths[0]}. Select the environment that contains it, then retry.`,
      status: 409,
    };
  }

  if (selection.kind === "server_sandbox") {
    return {
      code: "workspace_path_mismatch",
      error: `The selected Server sandbox is an isolated workspace and cannot access host path: ${paths[0]}. Use a relative path inside the sandbox, or select an Edge workspace rooted at that host path.`,
      status: 409,
    };
  }

  const outsidePath = paths.find((path) => !edgeWorkspaceOwnsPath(selection, path));
  if (!outsidePath) {
    return null;
  }
  return {
    code: "workspace_path_mismatch",
    error: `The referenced path is outside the selected file environment: ${outsidePath}. Choose the environment that contains it or use a path inside the current one.`,
    status: 409,
  };
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
      workspaceBinding: serverSandboxWorkspaceBinding(selection),
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
