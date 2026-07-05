import {
  defaultWorkspaceBinding,
  defaultExecutorBinding,
  edgeWorkspaceBinding,
  edgeExecutorBinding,
  normalizeWorkspaceSelection,
  sameWorkspaceSelection,
  normalizeSlashPath,
  resolveWorkspaceBindings,
} from "@/lib/workspace-authority";
import type { WorkspaceSelection } from "@/lib/api/types";

function edgeSelection(overrides: Partial<EdgeSelection> = {}): EdgeSelection {
  return {
    kind: "edge_workspace",
    edgeAgentId: "agent-1",
    cwd: "/Users/test/project",
    displayName: "Test Mac",
    ...overrides,
  };
}

type EdgeSelection = Extract<WorkspaceSelection, { kind: "edge_workspace" }>;

// ─── binding factories ──────────────────────────────────────────────

describe("defaultWorkspaceBinding", () => {
  it("returns default Web binding without exposing an absent workspace", () => {
    expect(defaultWorkspaceBinding()).toEqual({
      kind: "none",
      display_name: "Web",
      authority: "none",
      fallback_policy: "disabled",
    });
  });
});

describe("defaultExecutorBinding", () => {
  it("returns default server control-plane executor binding", () => {
    expect(defaultExecutorBinding()).toEqual({
      kind: "server_local",
      executor_id: "server-control-plane",
      display_name: "Server control plane",
      transport: "server_local",
      status: "online",
    });
  });
});

describe("edgeWorkspaceBinding", () => {
  it("maps an edge selection to a workspace binding", () => {
    expect(edgeWorkspaceBinding(edgeSelection())).toEqual({
      kind: "edge_workspace",
      display_name: "Test Mac",
      cwd: "/Users/test/project",
      authority: "read_write",
      fallback_policy: "disabled",
    });
  });

  it("falls back to edgeAgentId when displayName is missing", () => {
    expect(
      edgeWorkspaceBinding(edgeSelection({ displayName: null })),
    ).toHaveProperty("display_name", "agent-1");
  });
});

describe("edgeExecutorBinding", () => {
  it("maps an edge selection to an executor binding", () => {
    expect(edgeExecutorBinding(edgeSelection())).toEqual({
      kind: "edge_agent",
      executor_id: "agent-1",
      display_name: "Test Mac",
      transport: "edge_ws",
      status: "online",
    });
  });
});

// ─── normalizeWorkspaceSelection ────────────────────────────────────

describe("normalizeWorkspaceSelection", () => {
  it("returns undefined for non-objects", () => {
    expect(normalizeWorkspaceSelection(null)).toBeUndefined();
    expect(normalizeWorkspaceSelection(undefined)).toBeUndefined();
    expect(normalizeWorkspaceSelection("server_sandbox")).toBeUndefined();
    expect(normalizeWorkspaceSelection(42)).toBeUndefined();
    expect(normalizeWorkspaceSelection([])).toBeUndefined();
  });

  it("returns server_sandbox selection", () => {
    expect(normalizeWorkspaceSelection({ kind: "server_sandbox" })).toEqual({
      kind: "server_sandbox",
    });
  });

  it("returns undefined for unknown kinds", () => {
    expect(normalizeWorkspaceSelection({ kind: "remote_ssh" })).toBeUndefined();
  });

  it("validates edge workspace fields", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edgeAgentId: "agent-1",
        cwd: "/Users/test",
      }),
    ).toEqual({
      kind: "edge_workspace",
      edgeAgentId: "agent-1",
      displayName: null,
      cwd: "/Users/test",
    });
  });

  it("accepts snake_case alias edge_agent_id", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edge_agent_id: "agent-2",
        cwd: "/home/test",
      }),
    ).toEqual({
      kind: "edge_workspace",
      edgeAgentId: "agent-2",
      displayName: null,
      cwd: "/home/test",
    });
  });

  it("trims whitespace from fields", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edgeAgentId: "  agent-3  ",
        cwd: "  /Users/test  ",
        displayName: "  My Mac  ",
      }),
    ).toEqual({
      kind: "edge_workspace",
      edgeAgentId: "agent-3",
      displayName: "My Mac",
      cwd: "/Users/test",
    });
  });

  it("rejects edge workspace with empty edgeAgentId", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edgeAgentId: "",
        cwd: "/Users/test",
      }),
    ).toBeUndefined();
  });

  it("rejects edge workspace with empty cwd", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edgeAgentId: "agent-1",
        cwd: "",
      }),
    ).toBeUndefined();
  });

  it("accepts snake_case display_name alias", () => {
    expect(
      normalizeWorkspaceSelection({
        kind: "edge_workspace",
        edgeAgentId: "agent-1",
        cwd: "/Users/test",
        display_name: "Dev Mac",
      }),
    ).toEqual({
      kind: "edge_workspace",
      edgeAgentId: "agent-1",
      displayName: "Dev Mac",
      cwd: "/Users/test",
    });
  });
});

// ─── sameWorkspaceSelection ─────────────────────────────────────────

describe("sameWorkspaceSelection", () => {
  it("considers both null/undefined as same", () => {
    expect(sameWorkspaceSelection(null, null)).toBe(true);
    expect(sameWorkspaceSelection(undefined, undefined)).toBe(true);
    expect(sameWorkspaceSelection(null, undefined)).toBe(true);
  });

  it("considers null vs a selection as different", () => {
    expect(sameWorkspaceSelection(null, { kind: "server_sandbox" })).toBe(
      false,
    );
  });

  it("considers different kinds as different", () => {
    expect(
      sameWorkspaceSelection({ kind: "server_sandbox" }, edgeSelection()),
    ).toBe(false);
  });

  it("considers any two server_sandbox selections as same", () => {
    expect(
      sameWorkspaceSelection(
        { kind: "server_sandbox" },
        { kind: "server_sandbox" },
      ),
    ).toBe(true);
  });

  it("compares edge workspace fields", () => {
    const a = edgeSelection();
    const b = edgeSelection();
    expect(sameWorkspaceSelection(a, b)).toBe(true);

    expect(
      sameWorkspaceSelection(a, edgeSelection({ edgeAgentId: "agent-2" })),
    ).toBe(false);
    expect(sameWorkspaceSelection(a, edgeSelection({ cwd: "/other" }))).toBe(
      false,
    );
  });

  it("normalizes null/undefined displayName for comparison", () => {
    const a = edgeSelection({ displayName: null });
    const b: EdgeSelection = {
      kind: "edge_workspace",
      edgeAgentId: "agent-1",
      cwd: "/Users/test/project",
    };
    // Both null and undefined displayName normalize to null via ?? null
    expect(sameWorkspaceSelection(a, b)).toBe(true);
  });
});

// ─── normalizeSlashPath ─────────────────────────────────────────────

describe("normalizeSlashPath", () => {
  it("collapses double slashes", () => {
    expect(normalizeSlashPath("/Users//test//project")).toBe(
      "/Users/test/project",
    );
  });

  it("resolves . segments", () => {
    expect(normalizeSlashPath("/Users/./test/./project")).toBe(
      "/Users/test/project",
    );
  });

  it("resolves .. segments", () => {
    expect(normalizeSlashPath("/Users/test/project/../other")).toBe(
      "/Users/test/other",
    );
  });

  it("converts backslashes to forward slashes", () => {
    expect(normalizeSlashPath("C:\\Users\\test\\project")).toBe(
      "C:/Users/test/project",
    );
  });

  it("handles .. beyond root", () => {
    expect(normalizeSlashPath("/Users/../..")).toBe("/");
  });

  it("preserves trailing slash (implicitly)", () => {
    // normalizeSlashPath removes trailing empty segments
    expect(normalizeSlashPath("/Users/test/")).toBe("/Users/test");
  });

  it("returns empty string for root path made of only ..", () => {
    expect(normalizeSlashPath("../..")).toBe("");
  });
});

// ─── resolveWorkspaceBindings ───────────────────────────────────────

describe("resolveWorkspaceBindings", () => {
  it("returns no-file-environment bindings for null selection", () => {
    const result = resolveWorkspaceBindings(null);
    expect(result.workspaceBinding.kind).toBe("none");
    expect(result.executorBinding.kind).toBe("server_local");
    expect(result.executorBinding.executor_id).toBe("server-control-plane");
    expect(result.edgeProfile).toBeUndefined();
  });

  it("returns default bindings for server_sandbox selection", () => {
    const result = resolveWorkspaceBindings({ kind: "server_sandbox" });
    expect(result.workspaceBinding.kind).toBe("server_sandbox");
    expect(result.edgeProfile).toBeUndefined();
  });

  it("returns edge bindings for edge workspace selection", () => {
    const result = resolveWorkspaceBindings(edgeSelection());
    expect(result.workspaceBinding.kind).toBe("edge_workspace");
    expect(result.executorBinding.kind).toBe("edge_agent");
    expect(result.edgeProfile).toEqual({
      cwd: "/Users/test/project",
      edge_agent_id: "agent-1",
    });
  });
});
