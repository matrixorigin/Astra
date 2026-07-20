import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { ModelSwitcher } from "@/components/app/model-switcher";
import {
  listModels,
  type ModelCatalogResponse,
} from "@/lib/api/models";
import type { ModelSummary } from "@/lib/api/types";

vi.mock("@/lib/api/models", () => ({
  listModels: vi.fn(),
}));

const mockListModels = vi.mocked(listModels);

function modelCatalog(
  items: ModelSummary[],
  defaultOfferingId: string | null = items[0]?.id ?? null,
  actions: ModelCatalogResponse["actions"] = [],
): ModelCatalogResponse {
  return {
    items,
    accesses: [],
    defaultOfferingId,
    catalogRevision: "sha256:catalog",
    observedAt: "2026-07-20T00:00:00Z",
    source: "astra",
    status: items.length > 0 ? "ready" : "unavailable",
    actions,
  };
}

describe("ModelSwitcher", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("marks an unknown persisted model unavailable instead of replacing it", async () => {
    mockListModels.mockResolvedValue(
      modelCatalog([
        {
          id: "deepseek-v4-flash-anthropic",
          name: "deepseek-v4-flash-anthropic",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
        {
          id: "deepseek-v4-pro-official",
          name: "deepseek-v4-pro-official",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
      ]),
    );
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
    expect(trigger).toBeEnabled();
    expect(trigger).toHaveAttribute("title", "deepseek-v4-pro");
  });

  it("emits the selected canonical model name from the loaded list", async () => {
    mockListModels.mockResolvedValue(
      modelCatalog([
        {
          id: "deepseek-v4-flash-anthropic",
          name: "deepseek-v4-flash-anthropic",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
        {
          id: "deepseek-v4-pro-official",
          name: "deepseek-v4-pro-official",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
      ]),
    );
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

  it("selects the Server-governed default when no concrete model is set", async () => {
    mockListModels.mockResolvedValue(
      modelCatalog([
        {
          id: "deepseek-v4-flash-anthropic",
          name: "deepseek-v4-flash-anthropic",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
        {
          id: "deepseek-v4-pro-official",
          name: "deepseek-v4-pro-official",
          subtitle: "Self-hosted · Runs on server",
          tier: "included",
          accessLabel: "Self-hosted",
          executionPlacement: "server",
        },
      ], "deepseek-v4-pro-official"),
    );
    const onChange = vi.fn();
    const onModelAvailabilityChange = vi.fn();

    render(
      <ModelSwitcher
        value=""
        onChange={onChange}
        onModelAvailabilityChange={onModelAvailabilityChange}
        thinking={false}
        onThinkingChange={vi.fn()}
      />,
    );

    await waitFor(() =>
      expect(onChange).toHaveBeenCalledWith("deepseek-v4-pro-official"),
    );
    expect(
      screen.getByRole("button", { name: /deepseek-v4-pro-official/i }),
    ).not.toHaveAttribute("aria-invalid");
    await waitFor(() =>
      expect(onModelAvailabilityChange).toHaveBeenCalledWith(true),
    );
  });

  it("keeps selection unavailable when Model Access has no eligible Offering", async () => {
    mockListModels.mockResolvedValue(
      modelCatalog([], null, ["contact_administrator"]),
    );
    const onChange = vi.fn();
    const onModelAvailabilityChange = vi.fn();

    render(
      <ModelSwitcher
        value=""
        onChange={onChange}
        onModelAvailabilityChange={onModelAvailabilityChange}
        thinking={false}
        onThinkingChange={vi.fn()}
      />,
    );

    expect(await screen.findByText("No models available")).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "No models available" }));
    expect(
      await screen.findByText("Ask an administrator to enable a model."),
    ).toBeVisible();
    expect(onChange).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(onModelAvailabilityChange).toHaveBeenCalledWith(false),
    );
  });

  it("distinguishes catalog failure from an unavailable Offering", async () => {
    mockListModels.mockRejectedValue(new Error("catalog offline"));
    const onModelAvailabilityChange = vi.fn();

    render(
      <ModelSwitcher
        value=""
        onChange={vi.fn()}
        onModelAvailabilityChange={onModelAvailabilityChange}
        thinking={false}
        onThinkingChange={vi.fn()}
      />,
    );

    const trigger = await screen.findByRole("button", {
      name: "Model access unavailable",
    });
    fireEvent.click(trigger);
    expect(
      await screen.findByText(
        "Model Access could not be loaded. Sign in again or retry.",
      ),
    ).toBeVisible();
    await waitFor(() =>
      expect(onModelAvailabilityChange).toHaveBeenCalledWith(false),
    );
  });
});
