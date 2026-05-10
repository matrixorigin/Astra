'use client';

import { useEffect } from 'react';

export function useKeyboardShortcut(
  match: (event: KeyboardEvent) => boolean,
  handler: (event: KeyboardEvent) => void,
) {
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (match(event)) {
        handler(event);
      }
    }
    window.addEventListener('keydown', onKeyDown);
    return () => window.removeEventListener('keydown', onKeyDown);
  }, [handler, match]);
}
