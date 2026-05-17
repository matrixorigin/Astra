'use client';

import { ChevronDown } from 'lucide-react';
import type { ReactNode } from 'react';
import { useState } from 'react';
import { cn } from '@/lib/utils/cn';

export function SidebarSection({
  label,
  children,
  defaultCollapsed = false,
}: {
  label: string;
  children: ReactNode;
  defaultCollapsed?: boolean;
}) {
  const [collapsed, setCollapsed] = useState(defaultCollapsed);
  return (
    <section className="astra-sidebar-section mt-4">
      <button
        type="button"
        onClick={() => setCollapsed((value) => !value)}
        className="astra-sidebar-section-toggle flex w-full items-center justify-between px-3 text-xs font-medium"
      >
        <span>{label}</span>
        <ChevronDown className={cn('size-3', collapsed && '-rotate-90')} />
      </button>
      {collapsed ? null : <div className="astra-sidebar-section-content mt-2 space-y-1">{children}</div>}
    </section>
  );
}
