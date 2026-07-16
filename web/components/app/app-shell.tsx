"use client";

import {
  FolderKanban,
  Home,
  MessageSquare,
  Search,
  Workflow,
  type LucideIcon,
} from "lucide-react";
import Link from "next/link";
import { usePathname, useRouter } from "next/navigation";
import type { ReactNode } from "react";
import { useCallback, useState } from "react";
import { Sidebar } from "@/components/app/sidebar";
import { SearchModal } from "@/components/app/search-modal";
import { ToastProvider } from "@/components/ui/toast";
import { useKeyboardShortcut } from "@/hooks/use-keyboard-shortcut";
import { cn } from "@/lib/utils/cn";

export function AppShell({ children }: { children: ReactNode }) {
  const router = useRouter();
  const pathname = usePathname();
  const [searchOpen, setSearchOpen] = useState(false);

  const openSearch = useCallback(() => setSearchOpen(true), []);
  useKeyboardShortcut(
    useCallback(
      (event) =>
        (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k",
      [],
    ),
    useCallback((event) => {
      event.preventDefault();
      setSearchOpen((value) => !value);
    }, []),
  );
  useKeyboardShortcut(
    useCallback(
      (event) =>
        (event.metaKey || event.ctrlKey) &&
        event.shiftKey &&
        event.key.toLowerCase() === "o",
      [],
    ),
    useCallback(
      (event) => {
        event.preventDefault();
        router.push("/");
        window.setTimeout(() => {
          document
            .querySelector<HTMLTextAreaElement>('[data-composer-input="true"]')
            ?.focus();
        }, 50);
      },
      [router],
    ),
  );
  useKeyboardShortcut(
    useCallback(
      (event) => (event.metaKey || event.ctrlKey) && event.key === "/",
      [],
    ),
    useCallback((event) => {
      event.preventDefault();
      document
        .querySelector<HTMLTextAreaElement>('[data-composer-input="true"]')
        ?.focus();
    }, []),
  );

  return (
    <ToastProvider>
      <div className="flex h-[100dvh] overflow-hidden bg-bg text-text">
        <Sidebar onSearch={openSearch} />
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <header className="flex h-[52px] shrink-0 items-center gap-3 border-b border-border bg-surface px-3 md:hidden">
            <Link
              href="/"
              className="inline-flex min-w-0 items-center gap-2 text-sm font-semibold"
              aria-label="Astra home"
            >
              <span className="flex size-7 items-center justify-center rounded-control bg-text text-[10px] font-bold text-white">
                A
              </span>
              <span className="truncate">Astra</span>
            </Link>
            <div className="flex-1" />
            <button
              type="button"
              onClick={openSearch}
              className="inline-flex size-9 items-center justify-center rounded-control text-text-muted hover:bg-surface-muted hover:text-text"
              aria-label="Search workspace"
            >
              <Search className="size-4" />
            </button>
          </header>
          <main className="min-h-0 min-w-0 flex-1 overflow-hidden">
            {children}
          </main>
          <nav
            className="grid h-14 shrink-0 grid-cols-4 border-t border-border bg-surface px-1 md:hidden"
            aria-label="Mobile primary navigation"
          >
            <MobileNavItem
              href="/"
              label="New"
              icon={Home}
              active={pathname === "/"}
            />
            <MobileNavItem
              href="/chats"
              label="Chats"
              icon={MessageSquare}
              active={pathname === "/chats" || pathname.startsWith("/chats/")}
            />
            <MobileNavItem
              href="/projects"
              label="Projects"
              icon={FolderKanban}
              active={pathname === "/projects" || pathname.startsWith("/projects/")}
            />
            <MobileNavItem
              href="/harnesses"
              label="Harnesses"
              icon={Workflow}
              active={pathname === "/harnesses"}
            />
          </nav>
        </div>
        <SearchModal open={searchOpen} onOpenChange={setSearchOpen} />
      </div>
    </ToastProvider>
  );
}

function MobileNavItem({
  href,
  label,
  icon: Icon,
  active,
}: {
  href: string;
  label: string;
  icon: LucideIcon;
  active: boolean;
}) {
  return (
    <Link
      href={href}
      aria-current={active ? "page" : undefined}
      className={cn(
        "flex min-w-0 flex-col items-center justify-center gap-0.5 rounded-control text-[10px] font-medium",
        active ? "text-accent" : "text-text-muted hover:text-text",
      )}
    >
      <Icon className="size-4" />
      <span className="truncate">{label}</span>
    </Link>
  );
}
