import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { Composer } from "@/components/app/composer";
import { listModels } from "@/lib/api/models";

vi.mock("@/components/app/composer-plus-menu", () => ({
  ComposerEnvironmentChip: () => null,
  ComposerPlusMenu: () => null,
}));

vi.mock("@/components/app/slash-command-panel", () => ({
  SlashCommandPanel: () => null,
}));

vi.mock("@/components/ui/icon-button", () => ({
  IconButton: ({
    label,
    type = "button",
    disabled,
    onClick,
  }: {
    label: string;
    type?: "button" | "submit";
    disabled?: boolean;
    onClick?: () => void;
  }) => (
    <button type={type} aria-label={label} disabled={disabled} onClick={onClick}>
      {label}
    </button>
  ),
}));

vi.mock("@/hooks/use-skill-catalog", () => ({
  useSkillCatalog: () => ({
    items: [],
    loading: false,
    error: null,
    loadedAll: true,
    loadAll: vi.fn(),
  }),
}));

vi.mock("@/lib/api/models", () => ({
  listModels: vi.fn(),
}));

const mockListModels = vi.mocked(listModels);

const deepseekModels = [
  {
    id: "deepseek-v4-flash-anthropic",
    name: "deepseek-v4-flash-anthropic",
    subtitle: "anthropic",
    tier: "included" as const,
    accessLabel: "Self-hosted",
    executionPlacement: "server" as const,
  },
  {
    id: "deepseek-v4-pro-official",
    name: "deepseek-v4-pro-official",
    subtitle: "openai",
    tier: "included" as const,
    accessLabel: "Self-hosted",
    executionPlacement: "server" as const,
  },
];

describe("Composer model selection", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    window.localStorage.clear();
  });

  it("blocks submit for an unavailable persisted model until a canonical model is selected", async () => {
    window.localStorage.setItem("astra.composer.model", "deepseek-v4-pro");
    mockListModels.mockResolvedValue({
      items: deepseekModels,
      accesses: [],
      defaultOfferingId: "deepseek-v4-flash-anthropic",
      catalogRevision: "sha256:catalog",
      observedAt: "2026-07-20T00:00:00Z",
      source: "astra",
      status: "ready",
      actions: [],
    });
    const onSubmit = vi.fn().mockResolvedValue(undefined);

    render(<Composer onSubmit={onSubmit} />);

    const editor = screen.getByRole("textbox", {
      name: /how can i help you today/i,
    });
    editor.textContent = "hello";
    fireEvent.input(editor);

    const send = screen.getByRole("button", { name: "Send message" });
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: /unavailable model/i }),
      ).toHaveAttribute("title", "deepseek-v4-pro"),
    );
    expect(send).toBeDisabled();

    fireEvent.click(send);
    fireEvent.keyDown(editor, { key: "Enter", code: "Enter" });
    expect(onSubmit).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: /unavailable model/i }));
    fireEvent.click(
      await screen.findByRole("button", {
        name: /deepseek-v4-pro-official/i,
      }),
    );

    await waitFor(() => expect(send).not.toBeDisabled());
    fireEvent.click(send);

    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit).toHaveBeenCalledWith({
      text: "hello",
      attachments: [],
      options: {
        webSearch: false,
        thinking: true,
        model: "deepseek-v4-pro-official",
        activeSkills: [],
        activeTools: [],
      },
    });
  });
});
