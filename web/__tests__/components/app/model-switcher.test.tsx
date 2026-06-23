import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ModelSwitcher } from "@/components/app/model-switcher";
import { listModels } from "@/lib/api/models";

vi.mock("@/lib/api/models", () => ({
  listModels: vi.fn(),
}));

const mockListModels = vi.mocked(listModels);

describe("ModelSwitcher", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("marks an unknown persisted model unavailable instead of replacing it", async () => {
    mockListModels.mockResolvedValue({
      items: [
        {
          id: "deepseek-v4-flash-anthropic",
          name: "deepseek-v4-flash-anthropic",
          subtitle: "anthropic",
          tier: "included",
        },
        {
          id: "deepseek-v4-pro-official",
          name: "deepseek-v4-pro-official",
          subtitle: "openai",
          tier: "included",
        },
      ],
    });
    const onChange = vi.fn();
    const onModelAvailabilityChange = vi.fn();

    render(
      <ModelSwitcher
        value="deepseek-v4-pro"
        onChange={onChange}
        onModelAvailabilityChange={onModelAvailabilityChange}
        thinking={false}
        onThinkingChange={vi.fn()}
      />,
    );

    await waitFor(() => expect(mockListModels).toHaveBeenCalledTimes(1));

    expect(onChange).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(onModelAvailabilityChange).toHaveBeenCalledWith(false),
    );
    const trigger = screen.getByRole("button", { name: /unavailable model/i });
    expect(trigger).toHaveAttribute("aria-invalid", "true");
    expect(trigger).toHaveAttribute("title", "deepseek-v4-pro");
  });

  it("emits the selected canonical model name from the loaded list", async () => {
    mockListModels.mockResolvedValue({
      items: [
        {
          id: "deepseek-v4-flash-anthropic",
          name: "deepseek-v4-flash-anthropic",
          subtitle: "anthropic",
          tier: "included",
        },
        {
          id: "deepseek-v4-pro-official",
          name: "deepseek-v4-pro-official",
          subtitle: "openai",
          tier: "included",
        },
      ],
    });
    const onChange = vi.fn();

    render(
      <ModelSwitcher
        value="deepseek-v4-flash-anthropic"
        onChange={onChange}
        thinking={false}
        onThinkingChange={vi.fn()}
      />,
    );

    await waitFor(() => expect(mockListModels).toHaveBeenCalledTimes(1));
    fireEvent.click(
      screen.getByRole("button", { name: /deepseek-v4-flash-anthropic/i }),
    );
    fireEvent.click(
      await screen.findByRole("button", {
        name: /deepseek-v4-pro-official/i,
      }),
    );

    expect(onChange).toHaveBeenCalledWith("deepseek-v4-pro-official");
  });
});
