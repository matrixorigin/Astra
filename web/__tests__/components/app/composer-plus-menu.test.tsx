import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ComponentProps } from "react";
import {
  ComposerEnvironmentChip,
  ComposerPlusMenu,
} from "@/components/app/composer-plus-menu";
import type { WorkspaceSelection } from "@/lib/api/types";

vi.mock("lucide-react", () => {
  const Icon = ({ className }: { className?: string }) => (
    <span data-testid="icon" className={className} />
  );
  return {
    __esModule: true,
    AlertTriangle: Icon,
    ChevronLeft: Icon,
    Check: Icon,
    FilePlus2: Icon,
    Globe: Icon,
    GitPullRequest: Icon,
    HardDrive: Icon,
    Image: Icon,
    Monitor: Icon,
    Plug: Icon,
    Puzzle: Icon,
    RefreshCw: Icon,
    SlidersHorizontal: Icon,
    SquarePlus: Icon,
  };
});

vi.mock("@/components/app/skill-picker-panel", () => ({
  SkillPickerPanel: () => <div>Skill picker</div>,
}));

function renderMenu(
  props: Partial<ComponentProps<typeof ComposerPlusMenu>> = {},
) {
  return render(
    <ComposerPlusMenu
      webSearch={false}
      onWebSearchChange={vi.fn()}
      webAccess={{
        available: true,
        description: "Run via Server",
        provider: {
          provider_id: "server-builtin",
          kind: "server",
          display_name: "Server",
          status: "ready",
        },
      }}
      githubAccess={{
        available: true,
        description: "Run via Server",
        provider: {
          provider_id: "server-builtin",
          kind: "server",
          display_name: "Server",
          status: "ready",
        },
      }}
      activeSkills={[]}
      onActiveSkillsChange={vi.fn()}
      {...props}
    />,
  );
}

function renderEnvironmentChip(
  props: Partial<ComponentProps<typeof ComposerEnvironmentChip>> = {},
) {
  return render(<ComposerEnvironmentChip {...props} />);
}

describe("ComposerPlusMenu environment selection", () => {
  it("lets the user bind a connected edge workspace", async () => {
    const user = userEvent.setup();
    const onWorkspaceSelectionChange = vi.fn();
    renderMenu({
      edgeWorkspaces: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 8,
        },
      ],
      onWorkspaceSelectionChange,
    });

    await user.click(screen.getByRole("button", { name: "Open add menu" }));
    await user.click(screen.getByRole("button", { name: /Environment/i }));
    await user.click(screen.getByText("MacBook Pro").closest("button")!);

    expect(onWorkspaceSelectionChange).toHaveBeenCalledWith({
      kind: "edge_workspace",
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
    });
  });

  it("lets the user clear an edge binding back to Web without workspace", async () => {
    const user = userEvent.setup();
    const onWorkspaceSelectionChange = vi.fn();
    const workspaceSelection: WorkspaceSelection = {
      kind: "edge_workspace",
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
    };
    renderMenu({
      workspaceSelection,
      edgeWorkspaces: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 8,
        },
      ],
      onWorkspaceSelectionChange,
    });

    await user.click(screen.getByRole("button", { name: "Open add menu" }));
    await user.click(screen.getByRole("button", { name: /Environment/i }));
    await user.click(screen.getByText("Web").closest("button")!);

    expect(onWorkspaceSelectionChange).toHaveBeenCalledWith(null);
  });

  it("shows a durable edge binding when that edge is offline", async () => {
    const user = userEvent.setup();
    renderMenu({
      workspaceSelection: {
        kind: "edge_workspace",
        edgeAgentId: "edge-1",
        displayName: "MacBook Pro",
        cwd: "/Users/test/astra",
      },
      edgeWorkspaces: [],
    });

    await user.click(screen.getByRole("button", { name: "Open add menu" }));
    await user.click(screen.getByRole("button", { name: /Environment/i }));

    expect(screen.getByText("Bound edge is offline")).toBeInTheDocument();
    expect(screen.getByText("MacBook Pro · /Users/test/astra")).toBeInTheDocument();
  });

  it("treats a restarted edge with the same workspace as connected", async () => {
    const user = userEvent.setup();
    renderMenu({
      workspaceSelection: {
        kind: "edge_workspace",
        edgeAgentId: "edge-old-random",
        displayName: "MacBook Pro",
        cwd: "/Users/test/astra",
      },
      edgeWorkspaces: [
        {
          edge_agent_id: "edge-new-stable",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 3,
        },
      ],
    });

    await user.click(screen.getByRole("button", { name: "Open add menu" }));
    await user.click(screen.getByRole("button", { name: /Environment/i }));

    expect(screen.queryByText("Bound edge is offline")).not.toBeInTheDocument();
    expect(screen.getByText("MacBook Pro")).toBeInTheDocument();
  });
});

describe("ComposerPlusMenu connectors", () => {
  it("lets the user attach the GitHub connector for the next turn", async () => {
    const user = userEvent.setup();
    const onActiveToolsChange = vi.fn();
    renderMenu({ activeTools: [], onActiveToolsChange });

    await user.click(screen.getByRole("button", { name: "Open add menu" }));
    await user.click(screen.getByRole("button", { name: /Connectors/i }));
    await user.click(screen.getByRole("button", { name: /GitHub/i }));

    expect(onActiveToolsChange).toHaveBeenCalledWith(["github"]);
    expect(
      screen.getByText(/credentials configured on the selected server or edge/i),
    ).toBeInTheDocument();
  });
});

describe("ComposerEnvironmentChip", () => {
  it("shows the selected edge workspace as persistent composer context", () => {
    renderEnvironmentChip({
      workspaceSelection: {
        kind: "edge_workspace",
        edgeAgentId: "edge-1",
        displayName: "MacBook Pro",
        cwd: "/Users/test/astra",
      },
      edgeWorkspaces: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 8,
        },
      ],
    });

    expect(
      screen.getByRole("button", { name: "Environment: MacBook Pro" }),
    ).toBeInTheDocument();
    expect(screen.getByText("MacBook Pro")).toBeInTheDocument();
    expect(screen.getByText("/Users/test/astra")).toBeInTheDocument();
  });

  it("opens the same explicit environment picker from the persistent chip", async () => {
    const user = userEvent.setup();
    const onWorkspaceSelectionChange = vi.fn();
    renderEnvironmentChip({
      edgeWorkspaces: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 8,
        },
      ],
      onWorkspaceSelectionChange,
    });

    await user.click(screen.getByRole("button", { name: "Environment: Web" }));
    await user.click(screen.getByText("MacBook Pro").closest("button")!);

    expect(onWorkspaceSelectionChange).toHaveBeenCalledWith({
      kind: "edge_workspace",
      edgeAgentId: "edge-1",
      displayName: "MacBook Pro",
      cwd: "/Users/test/astra",
    });
  });
});
