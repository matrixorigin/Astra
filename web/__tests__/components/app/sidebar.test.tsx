import { render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { Sidebar } from "@/components/app/sidebar";

vi.mock("next/link", () => ({
  default: ({ href, children, ...props }: { href: string; children: ReactNode }) => (
    <a href={href} {...props}>
      {children}
    </a>
  ),
}));

vi.mock("next/navigation", () => ({
  usePathname: () => "/chats",
}));

vi.mock("lucide-react", () => {
  const Icon = () => null;
  return {
    Box: Icon,
    ChevronLeft: Icon,
    ChevronRight: Icon,
    Info: Icon,
    MessageSquare: Icon,
    MoreHorizontal: Icon,
    Trash2: Icon,
  };
});

vi.mock("@/components/app/chat-actions-menu", () => ({
  ChatActionsMenu: () => null,
}));

vi.mock("@/components/app/tui-entity-mark", () => ({
  TuiEntityMark: () => null,
}));

vi.mock("@/components/app/user-badge", () => ({
  UserBadge: () => null,
}));

vi.mock("@/components/ui/icon-button", () => ({
  IconButton: ({ label, onClick }: { label: string; onClick: () => void }) => (
    <button type="button" aria-label={label} onClick={onClick} />
  ),
}));

vi.mock("@/components/ui/modal", () => ({
  Modal: ({ children }: { children: ReactNode }) => children,
}));

vi.mock("@/components/ui/sidebar-section", () => ({
  SidebarSection: ({ children }: { children: ReactNode }) => children,
}));

vi.mock("@/hooks/use-chat-lifecycle-actions", () => ({
  useChatLifecycleActions: () => ({
    clearingArchived: false,
    clearArchived: vi.fn(),
  }),
}));

vi.mock("@/hooks/use-sidebar", () => ({
  useSidebar: () => ({ collapsed: false, toggle: vi.fn() }),
}));

vi.mock("@/lib/chat-lifecycle-events", () => ({
  subscribeChatLifecycleChange: () => () => undefined,
}));

vi.mock("@/lib/api/sidebar", () => ({
  getSidebarData: vi.fn().mockResolvedValue({
    recents: [],
    recentProjectGroups: [],
    recentOtherChats: [],
    untitled: [],
    archivedChats: [],
    user: { id: "u", name: "User", plan: "free" },
  }),
}));

vi.mock("@/lib/api/chats", () => ({
  getChat: vi.fn(),
}));

test("keeps Chat and Work as separate desktop entrypoints", () => {
  render(<Sidebar onSearch={vi.fn()} />);

  expect(screen.getByRole("link", { name: "New Work" })).toHaveAttribute("href", "/works");
  expect(screen.getByRole("link", { name: "New chat" })).toHaveAttribute("href", "/");
  expect(screen.getByRole("link", { name: "Chats" })).toHaveAttribute("href", "/chats");
  expect(screen.getByRole("link", { name: "Work" })).toHaveAttribute("href", "/works");
});
