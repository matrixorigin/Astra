import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    HTMLElement.prototype.scrollBy = vi.fn();
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
      await screen.findByRole("button", { name: "Response copied" }),
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

  it("does not flash Thought during a transient reasoning completion", async () => {
    vi.useFakeTimers();
    try {
      const { rerender } = render(
        <MessageBubble
          message={assistantMessage({
            reasoning: "Finished one internal reasoning segment.",
            reasoningStatus: "streaming",
            status: "streaming",
          })}
        />,
      );

      expect(screen.getByText(/^Thinking/)).toBeInTheDocument();

      rerender(
        <MessageBubble
          message={assistantMessage({
            reasoning: "Finished one internal reasoning segment.",
            reasoningStatus: "complete",
            status: "complete",
            completedAt: null,
          })}
        />,
      );

      expect(screen.getByText(/^Thinking/)).toBeInTheDocument();
      expect(screen.queryByText(/^Thought/)).not.toBeInTheDocument();

      act(() => {
        vi.advanceTimersByTime(899);
      });
      expect(screen.getByText(/^Thinking/)).toBeInTheDocument();

      act(() => {
        vi.advanceTimersByTime(1);
      });
      expect(screen.getByText(/^Thought/)).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not force-scroll reasoning after manual scrollback", async () => {
    const scrollTo = vi.fn();
    const { rerender } = render(
      <MessageBubble
        message={assistantMessage({
          reasoning: "First reasoning line.\n\nSecond reasoning line.",
          reasoningStatus: "streaming",
          status: "streaming",
        })}
      />,
    );

    const scroller = screen.getByTestId("reasoning-scroll-container");
    Object.defineProperties(scroller, {
      scrollHeight: { configurable: true, value: 1000 },
      scrollTop: { configurable: true, value: 300 },
      clientHeight: { configurable: true, value: 240 },
      scrollTo: { configurable: true, value: scrollTo },
    });
    fireEvent.wheel(scroller, { deltaY: -120 });
    scrollTo.mockClear();

    rerender(
      <MessageBubble
        message={assistantMessage({
          reasoning:
            "First reasoning line.\n\nSecond reasoning line.\n\nNew reasoning line.",
          reasoningStatus: "streaming",
          status: "streaming",
        })}
      />,
    );

    expect(screen.getByText("New reasoning line.")).toBeInTheDocument();
    expect(scrollTo).not.toHaveBeenCalled();
  });

  it("forwards streaming reasoning wheel gestures to the chat scroller", () => {
    const scrollBy = vi.fn();
    const { container } = render(
      <div data-chat-scroll-container="true">
        <MessageBubble
          message={assistantMessage({
            reasoning: "Streaming reasoning line.",
            reasoningStatus: "streaming",
            status: "streaming",
          })}
        />
      </div>,
    );
    const chatScroller = container.querySelector(
      '[data-chat-scroll-container="true"]',
    ) as HTMLElement;
    Object.defineProperty(chatScroller, "scrollBy", {
      configurable: true,
      value: scrollBy,
    });

    fireEvent.wheel(screen.getByTestId("reasoning-scroll-container"), {
      deltaY: 120,
    });

    expect(scrollBy).toHaveBeenCalledWith({
      top: 120,
      left: 0,
      behavior: "auto",
    });
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
