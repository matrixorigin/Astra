import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { renderToString } from "react-dom/server";
import { MessageBubble } from "@/components/app/message-bubble";
import type { ChatMessage } from "@/lib/api/types";

vi.mock("react-markdown", () => ({
  __esModule: true,
  default: ({ children }: { children: string }) => <div>{children}</div>,
}));
vi.mock("rehype-highlight", () => ({
  __esModule: true,
  default: vi.fn(),
}));
vi.mock("rehype-katex", () => ({
  __esModule: true,
  default: vi.fn(),
}));
vi.mock("remark-gfm", () => ({
  __esModule: true,
  default: vi.fn(),
}));
vi.mock("remark-math", () => ({
  __esModule: true,
  default: vi.fn(),
}));

function assistantMessage(overrides: Partial<ChatMessage> = {}): ChatMessage {
  return {
    id: "assistant-1",
    role: "assistant",
    content: "",
    createdAt: new Date().toISOString(),
    status: "streaming",
    reasoning: "",
    reasoningStatus: "streaming",
    ...overrides,
  };
}

describe("MessageBubble", () => {
  beforeEach(() => {
    HTMLElement.prototype.scrollTo = vi.fn();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: {
        writeText: vi.fn().mockResolvedValue(undefined),
      },
    });
  });

  it("shows only the typing indicator while a response has no visible content yet", () => {
    render(<MessageBubble message={assistantMessage()} />);

    expect(
      screen.getByRole("status", { name: "Astra is responding" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("Working")).not.toBeInTheDocument();
    expect(screen.queryByText("Thinking")).not.toBeInTheDocument();
    expect(screen.queryByText("Preparing response...")).not.toBeInTheDocument();
  });

  it("does not render settled empty assistant messages", () => {
    const { container } = render(
      <MessageBubble
        message={assistantMessage({
          status: "complete",
          reasoningStatus: undefined,
        })}
      />,
    );

    expect(container).toBeEmptyDOMElement();
  });

  it("shows the reasoning panel once reasoning content is available", async () => {
    render(
      <MessageBubble
        message={assistantMessage({
          reasoning: "Checking the execution boundary.",
        })}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText(/^Thinking \d+s$/)).toBeInTheDocument();
    });
    expect(
      screen.getAllByText("Checking the execution boundary.").length,
    ).toBeGreaterThan(0);
  });

  it("uses stable collapsed copy for completed reasoning", () => {
    render(
      <MessageBubble
        message={assistantMessage({
          reasoning: "Checking the execution boundary.",
          reasoningStatus: "complete",
          status: "complete",
          createdAt: "2026-06-11T00:00:00.000Z",
          completedAt: "2026-06-11T00:00:12.000Z",
        })}
      />,
    );

    const toggle = screen.getByRole("button", {
      name: /Thought 12s/,
    });
    expect(toggle).toHaveAttribute("aria-expanded", "false");

    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.queryByText("Done")).not.toBeInTheDocument();
  });

  it("copies completed assistant text and does not expose unwired actions", async () => {
    render(
      <MessageBubble
        message={assistantMessage({
          content: "Final answer.",
          reasoningStatus: undefined,
          status: "complete",
        })}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "Regenerate response" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Good response" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Bad response" }),
    ).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Copy response" }));

    await waitFor(() => {
      expect(navigator.clipboard.writeText).toHaveBeenCalledWith(
        "Final answer.",
      );
    });
    expect(
      screen.getByRole("button", { name: "Response copied" }),
    ).toBeInTheDocument();
  });

  it("keeps thinking copy while the assistant message is still streaming", async () => {
    render(
      <MessageBubble
        message={assistantMessage({
          reasoning: "Finished one internal reasoning segment.",
          reasoningStatus: "complete",
          status: "streaming",
        })}
      />,
    );

    await waitFor(() => {
      expect(screen.getByText(/^Thinking \d+s$/)).toBeInTheDocument();
    });
    expect(screen.queryByText(/^Thought/)).not.toBeInTheDocument();
  });

  it("does not render live elapsed time into streaming reasoning SSR markup", () => {
    const html = renderToString(
      <MessageBubble
        message={assistantMessage({
          reasoning: "Checking the execution boundary.",
          createdAt: "2026-06-11T00:00:00.000Z",
        })}
      />,
    );

    expect(html).toContain("Thinking");
    expect(html).not.toMatch(/Thinking \d+s/);
  });
});
