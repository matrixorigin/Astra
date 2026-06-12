import {
  defaultWorkspaceBinding,
  defaultExecutorBinding,
  edgeWorkspaceBinding,
  edgeExecutorBinding,
  normalizeWorkspaceSelection,
  sameWorkspaceSelection,
  localCodeIntent,
  extractLocalPathMentions,
  normalizeSlashPath,
  edgeWorkspaceOwnsPath,
  validateWorkspaceAuthority,
  resolveWorkspaceBindings,
  type WorkspaceAuthorityError,
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
  it("returns server_sandbox binding", () => {
    expect(defaultWorkspaceBinding()).toEqual({
      kind: "server_sandbox",
      display_name: "Server sandbox",
      authority: "read_write",
      fallback_policy: "disabled",
    });
  });
});

describe("defaultExecutorBinding", () => {
  it("returns server_local executor binding", () => {
    expect(defaultExecutorBinding()).toEqual({
      kind: "server_local",
      executor_id: "server-local",
      display_name: "Server sandbox",
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

// ─── localCodeIntent ────────────────────────────────────────────────

describe("localCodeIntent", () => {
  it("detects git status as local intent", () => {
    expect(localCodeIntent("run git status")).toBe(true);
  });

  it("detects npm test as local intent", () => {
    expect(localCodeIntent("please npm test")).toBe(true);
  });

  it("detects cargo build as local intent", () => {
    expect(localCodeIntent("cargo build --release")).toBe(true);
  });

  it("detects pytest as local intent", () => {
    expect(localCodeIntent("run pytest")).toBe(true);
  });

  it("detects go test as local intent", () => {
    expect(localCodeIntent("go test ./...")).toBe(true);
  });

  it("detects make targets as local intent", () => {
    expect(localCodeIntent("make build")).toBe(true);
  });

  it("detects local path mentions", () => {
    expect(localCodeIntent("fix /Users/test/project/src/main.ts")).toBe(true);
  });

  it('detects "this repo" context as local intent', () => {
    expect(localCodeIntent("review this repo")).toBe(true);
  });

  it('detects "current workspace" context as local intent', () => {
    expect(localCodeIntent("inspect current workspace")).toBe(true);
  });

  it("returns false for generic questions", () => {
    expect(localCodeIntent("what is rust")).toBe(false);
  });

  it("returns false for server-only operations", () => {
    expect(localCodeIntent("deploy to production")).toBe(false);
  });

  it("handles empty string", () => {
    expect(localCodeIntent("")).toBe(false);
  });

  it("handles whitespace-only string", () => {
    expect(localCodeIntent("   ")).toBe(false);
  });
});

// ─── extractLocalPathMentions ───────────────────────────────────────

describe("extractLocalPathMentions", () => {
  it("extracts absolute macOS paths", () => {
    expect(
      extractLocalPathMentions("fix /Users/test/project/src/main.ts"),
    ).toEqual(["/Users/test/project/src/main.ts"]);
  });

  it("extracts home-relative paths", () => {
    expect(extractLocalPathMentions("edit ~/project/readme.md")).toEqual([
      "~/project/readme.md",
    ]);
  });

  it("extracts $HOME paths", () => {
    expect(extractLocalPathMentions("fix $HOME/project/src/lib.rs")).toEqual([
      "$HOME/project/src/lib.rs",
    ]);
  });

  it("extracts ${HOME} paths", () => {
    expect(extractLocalPathMentions("read ${HOME}/docs/guide.txt")).toEqual([
      "${HOME}/docs/guide.txt",
    ]);
  });

  it("extracts /home/ paths", () => {
    expect(extractLocalPathMentions("check /home/user/project")).toEqual([
      "/home/user/project",
    ]);
  });

  it("extracts Windows paths", () => {
    expect(extractLocalPathMentions("run C:\\Users\\test\\script.ps1")).toEqual(
      ["C:\\Users\\test\\script.ps1"],
    );
  });

  it("strips trailing punctuation", () => {
    expect(extractLocalPathMentions("see /Users/test/file.txt.")).toEqual([
      "/Users/test/file.txt",
    ]);
  });

  it("returns multiple matches", () => {
    expect(
      extractLocalPathMentions("compare ~/project/a.ts with /home/user/b.rs"),
    ).toEqual(["~/project/a.ts", "/home/user/b.rs"]);
  });

  it("returns empty array when no paths found", () => {
    expect(extractLocalPathMentions("hello world")).toEqual([]);
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

// ─── edgeWorkspaceOwnsPath ──────────────────────────────────────────

describe("edgeWorkspaceOwnsPath", () => {
  const sel = edgeSelection({ cwd: "/Users/test/project" });

  it("returns false for non-edge workspace", () => {
    expect(
      edgeWorkspaceOwnsPath({ kind: "server_sandbox" }, "/Users/test/project"),
    ).toBe(false);
  });

  it("returns true for an exact cwd match", () => {
    expect(edgeWorkspaceOwnsPath(sel, "/Users/test/project")).toBe(true);
  });

  it("returns true for a file inside cwd", () => {
    expect(edgeWorkspaceOwnsPath(sel, "/Users/test/project/src/main.ts")).toBe(
      true,
    );
  });

  it("returns false for a path outside cwd", () => {
    expect(edgeWorkspaceOwnsPath(sel, "/Users/other/file.ts")).toBe(false);
  });

  it("resolves ~/ paths against the cwd-derived home", () => {
    expect(edgeWorkspaceOwnsPath(sel, "~/project/src/lib.rs")).toBe(true);
  });

  it("resolves $HOME/ paths", () => {
    expect(edgeWorkspaceOwnsPath(sel, "$HOME/project/src/lib.rs")).toBe(true);
  });

  it("returns false when home can't be derived from cwd", () => {
    const selNoHome = edgeSelection({ cwd: "/opt/app" });
    expect(edgeWorkspaceOwnsPath(selNoHome, "~/file.txt")).toBe(false);
  });

  it("normalizes both cwd and path before comparison", () => {
    const selMessy = edgeSelection({
      cwd: "/Users//test/./project/other/..",
    });
    expect(
      edgeWorkspaceOwnsPath(selMessy, "/Users/test/project/src//./lib.rs"),
    ).toBe(true);
  });
});

// ─── validateWorkspaceAuthority ─────────────────────────────────────

describe("validateWorkspaceAuthority", () => {
  it("returns null when there is no local code intent", () => {
    expect(validateWorkspaceAuthority("hello world", null)).toBeNull();
  });

  it("returns workspace_required when selection is null and intent is local", () => {
    const err = validateWorkspaceAuthority(
      "run git status",
      null,
    ) as WorkspaceAuthorityError;
    expect(err.code).toBe("workspace_required");
  });

  it("returns workspace_required when selection is undefined", () => {
    const err = validateWorkspaceAuthority(
      "cargo build",
      undefined,
    ) as WorkspaceAuthorityError;
    expect(err.code).toBe("workspace_required");
  });

  it("returns workspace_local_code_on_server_sandbox for server sandbox", () => {
    const err = validateWorkspaceAuthority("npm test", {
      kind: "server_sandbox",
    }) as WorkspaceAuthorityError;
    expect(err.code).toBe("workspace_local_code_on_server_sandbox");
  });

  it("returns null for valid edge workspace with local intent and no foreign paths", () => {
    expect(
      validateWorkspaceAuthority("review this repo", edgeSelection()),
    ).toBeNull();
  });

  it("returns workspace_path_mismatch when a mentioned path is outside cwd", () => {
    const err = validateWorkspaceAuthority(
      "fix /Users/other/project/src/bug.ts",
      edgeSelection({ cwd: "/Users/test/project" }),
    ) as WorkspaceAuthorityError;
    expect(err.code).toBe("workspace_path_mismatch");
    expect(err.message).toContain("/Users/other/project/src/bug.ts");
  });

  it("allows paths owned by the workspace", () => {
    expect(
      validateWorkspaceAuthority(
        "fix /Users/test/project/src/bug.ts",
        edgeSelection({ cwd: "/Users/test/project" }),
      ),
    ).toBeNull();
  });

  it("validates ~/ paths as owned when home matches", () => {
    expect(
      validateWorkspaceAuthority(
        "edit ~/project/readme.md",
        edgeSelection({ cwd: "/Users/test/project" }),
      ),
    ).toBeNull();
  });
});

// ─── resolveWorkspaceBindings ───────────────────────────────────────

describe("resolveWorkspaceBindings", () => {
  it("returns default bindings for null selection", () => {
    const result = resolveWorkspaceBindings(null);
    expect(result.workspaceBinding.kind).toBe("server_sandbox");
    expect(result.executorBinding.kind).toBe("server_local");
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
