import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { WorkspaceSelection } from "@/lib/api/types";

const routerMock = vi.hoisted(() => ({
  push: vi.fn(),
  replace: vi.fn(),
}));

const apiMock = vi.hoisted(() => ({
  createChat: vi.fn(),
  getEdgeStatus: vi.fn(),
}));

vi.mock("next/navigation", () => ({
  useRouter: () => routerMock,
}));

vi.mock("@/lib/api/chats", () => apiMock);

vi.mock("lucide-react", () => {
  const Icon = ({ className }: { className?: string }) => (
    <span data-testid="icon" className={className} />
  );
  return {
    __esModule: true,
    BookOpen: Icon,
    Code2: Icon,
    Coffee: Icon,
    Lightbulb: Icon,
    PenLine: Icon,
  };
});

vi.mock("@/components/app/composer", () => ({
  Composer: (props: {
    edgeWorkspaces?: Array<{
      edge_agent_id?: string;
      hostname?: string;
      workspace_dir?: string;
    }>;
    workspaceSelection?: WorkspaceSelection | null;
    onWorkspaceSelectionChange?: (selection: WorkspaceSelection | null) => void;
    onSubmit: (input: {
      text: string;
      options: {
        model: string;
        webSearch: boolean;
        thinking: boolean;
        activeSkills: string[];
      };
    }) => Promise<void>;
  }) => {
    const firstEdge = props.edgeWorkspaces?.[0];
    const firstEdgeSelection: WorkspaceSelection | null =
      firstEdge?.edge_agent_id && firstEdge.workspace_dir
        ? {
            kind: "edge_workspace",
            edgeAgentId: firstEdge.edge_agent_id,
            displayName: firstEdge.hostname ?? firstEdge.edge_agent_id,
            cwd: firstEdge.workspace_dir,
          }
        : null;
    return (
      <div>
        <div data-testid="edge-count">{props.edgeWorkspaces?.length ?? 0}</div>
        <div data-testid="workspace-kind">
          {props.workspaceSelection?.kind ?? "none"}
        </div>
        <button
          type="button"
          onClick={() =>
            props.onWorkspaceSelectionChange?.(firstEdgeSelection)
          }
        >
          Select edge
        </button>
        <button
          type="button"
          onClick={() =>
            props.onSubmit({
              text: "list project files",
              options: {
                model: "sonnet-4.6-adaptive",
                webSearch: false,
                thinking: true,
                activeSkills: [],
              },
            })
          }
        >
          Submit
        </button>
      </div>
    );
  },
}));

describe("HomeScreen environment selection", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    apiMock.getEdgeStatus.mockResolvedValue({
      edges: [
        {
          edge_agent_id: "edge-1",
          hostname: "MacBook Pro",
          workspace_dir: "/Users/test/astra",
          connected_secs: 7,
        },
      ],
    });
    apiMock.createChat.mockResolvedValue({
      chatId: "chat-1",
      messageId: "message-1",
    });
  });

  it("loads edge workspaces before the first message and carries the selected workspace into chat creation", async () => {
    const { HomeScreen } = await import("@/components/app/home-screen");
    const user = userEvent.setup();

    render(<HomeScreen />);

    await waitFor(() =>
      expect(screen.getByTestId("edge-count")).toHaveTextContent("1"),
    );

    await user.click(screen.getByRole("button", { name: "Select edge" }));
    expect(screen.getByTestId("workspace-kind")).toHaveTextContent(
      "edge_workspace",
    );

    await user.click(screen.getByRole("button", { name: "Submit" }));

    expect(apiMock.createChat).toHaveBeenCalledWith(
      expect.objectContaining({
        workspaceSelection: {
          kind: "edge_workspace",
          edgeAgentId: "edge-1",
          displayName: "MacBook Pro",
          cwd: "/Users/test/astra",
        },
      }),
    );
    expect(routerMock.replace).toHaveBeenCalledWith("/chats/chat-1");
  });
});
