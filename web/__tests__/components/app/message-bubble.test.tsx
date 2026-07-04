import { fireEvent, render, screen } from "@testing-library/react";
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

  it("shows the reasoning panel once reasoning content is available", () => {
    render(
      <MessageBubble
        message={assistantMessage({
          reasoning: "Checking the execution boundary.",
        })}
      />,
    );

    expect(screen.getByText(/^Thinking \d+s$/)).toBeInTheDocument();
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
  });
});
