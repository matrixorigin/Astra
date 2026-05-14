'use client';

import { useRouter } from 'next/navigation';
import type { ReactNode } from 'react';
import { useCallback, useState } from 'react';
import { Sidebar } from '@/components/app/sidebar';
import { SearchModal } from '@/components/app/search-modal';
import { useKeyboardShortcut } from '@/hooks/use-keyboard-shortcut';

export function AppShell({ children }: { children: ReactNode }) {
  const router = useRouter();
  const [searchOpen, setSearchOpen] = useState(false);

  const openSearch = useCallback(() => setSearchOpen(true), []);
  useKeyboardShortcut(
    useCallback((event) => (event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k', []),
    useCallback((event) => {
      event.preventDefault();
      setSearchOpen((value) => !value);
    }, []),
  );
  useKeyboardShortcut(
    useCallback(
      (event) => (event.metaKey || event.ctrlKey) && event.shiftKey && event.key.toLowerCase() === 'o',
      [],
    ),
    useCallback((event) => {
      event.preventDefault();
      router.push('/');
      window.setTimeout(() => {
        document.querySelector<HTMLTextAreaElement>('[data-composer-input="true"]')?.focus();
      }, 50);
    }, [router]),
  );
  useKeyboardShortcut(
    useCallback((event) => (event.metaKey || event.ctrlKey) && event.key === '/', []),
    useCallback((event) => {
      event.preventDefault();
      document.querySelector<HTMLTextAreaElement>('[data-composer-input="true"]')?.focus();
    }, []),
  );

  return (
    <div className="flex h-[100dvh] overflow-hidden bg-bg text-text">
      <Sidebar onSearch={openSearch} />
      <main className="h-full min-h-0 min-w-0 flex-1 overflow-hidden">{children}</main>
      <SearchModal open={searchOpen} onOpenChange={setSearchOpen} />
    </div>
  );
}
