'use client';

import { useEffect, useState } from 'react';

const KEY = 'astra.sidebar.collapsed';

export function useSidebar() {
  const [collapsed, setCollapsed] = useState(false);

  useEffect(() => {
    setCollapsed(window.localStorage.getItem(KEY) === 'true');
  }, []);

  function toggle() {
    setCollapsed((value) => {
      const next = !value;
      window.localStorage.setItem(KEY, String(next));
      return next;
    });
  }

  return { collapsed, toggle };
}
