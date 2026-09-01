import { render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { AppShell } from "@/components/app/app-shell";

const router = { push: vi.fn() };

vi.mock("next/link", () => ({
  default: ({ href, children, ...props }: { href: string; children: ReactNode }) => (
    <a href={href} {...props}>
      {children}
    </a>
  ),
}));

vi.mock("next/navigation", () => ({
  usePathname: () => "/now",
  useRouter: () => router,
}));

vi.mock("@/components/app/sidebar", () => ({
  Sidebar: () => null,
}));

vi.mock("@/components/app/search-modal", () => ({
  SearchModal: () => null,
}));

vi.mock("@/components/ui/toast", () => ({
  ToastProvider: ({ children }: { children: ReactNode }) => children,
}));

vi.mock("@/hooks/use-keyboard-shortcut", () => ({
  useKeyboardShortcut: () => undefined,
}));

vi.mock("lucide-react", () => {
  const Icon = () => null;
  return {
    FolderKanban: Icon,
    Home: Icon,
    ListTodo: Icon,
    MessageSquare: Icon,
    Search: Icon,
    Workflow: Icon,
  };
});

test("keeps Chat as a first-class navigation surface beside Work", () => {
  render(<AppShell>content</AppShell>);

  expect(screen.getByRole("link", { name: "Now" })).toHaveAttribute("href", "/now");
  expect(screen.getByRole("link", { name: "Work" })).toHaveAttribute("href", "/works");
  expect(screen.getByRole("link", { name: "Chats" })).toHaveAttribute("href", "/chats");
  expect(screen.getByRole("link", { name: "Projects" })).toHaveAttribute("href", "/projects");
  expect(screen.getByRole("link", { name: "Harnesses" })).toHaveAttribute("href", "/harnesses");
});
