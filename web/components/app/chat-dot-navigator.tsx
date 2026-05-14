'use client';

import type { CSSProperties, RefObject } from 'react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { cn } from '@/lib/utils/cn';

type ChatDotNavigatorProps = {
  messageCount: number;
  scrollContainerRef: RefObject<HTMLDivElement | null>;
};

function computeStep(total: number) {
  if (total <= 30) {
    return 2;
  }
  if (total <= 100) {
    return 5;
  }
  if (total <= 300) {
    return 15;
  }
  if (total <= 600) {
    return 30;
  }
  if (total <= 1500) {
    return 50;
  }
  return 100;
}

function formatMessageLabel(index: number) {
  const number = index + 1;
  if (number >= 1000) {
    return `${(number / 1000).toFixed(number % 1000 === 0 ? 0 : 1)}k`;
  }
  return `Msg ${number}`;
}

export function ChatDotNavigator({ messageCount, scrollContainerRef }: ChatDotNavigatorProps) {
  const [activeIndex, setActiveIndex] = useState(0);
  const [progress, setProgress] = useState(0);
  const anchors = useMemo(() => {
    if (messageCount < 6) {
      return [];
    }
    const step = computeStep(messageCount);
    const values: number[] = [];
    for (let index = 0; index < messageCount; index += step) {
      values.push(index);
    }
    const lastIndex = messageCount - 1;
    if (values[values.length - 1] !== lastIndex) {
      values.push(lastIndex);
    }
    return values;
  }, [messageCount]);

  const update = useCallback(() => {
    const container = scrollContainerRef.current;
    if (!container) {
      return;
    }

    const maxScroll = Math.max(container.scrollHeight - container.clientHeight, 1);
    setProgress(Math.max(0, Math.min(1, container.scrollTop / maxScroll)));

    const containerTop = container.getBoundingClientRect().top;
    let current = 0;
    for (let index = 0; index < messageCount; index += 1) {
      const element = container.querySelector<HTMLElement>(`[data-chat-message-index="${index}"]`);
      if (!element) {
        continue;
      }
      const top = element.getBoundingClientRect().top - containerTop;
      if (top <= 120) {
        current = index;
      } else {
        break;
      }
    }
    setActiveIndex(current);
  }, [messageCount, scrollContainerRef]);

  useEffect(() => {
    const container = scrollContainerRef.current;
    if (!container || anchors.length === 0) {
      return undefined;
    }
    update();
    container.addEventListener('scroll', update, { passive: true });
    window.addEventListener('resize', update);
    return () => {
      container.removeEventListener('scroll', update);
      window.removeEventListener('resize', update);
    };
  }, [anchors.length, scrollContainerRef, update]);

  if (anchors.length === 0) {
    return null;
  }

  const activeAnchor = anchors.slice().reverse().find((index) => index <= activeIndex) ?? anchors[0];
  const style = { '--progress': `${progress * 100}%` } as CSSProperties;

  return (
    <nav className="astra-dot-nav" aria-label="Message navigation" style={style}>
      {anchors.map((index) => (
        <button
          key={index}
          type="button"
          className={cn('astra-dot-nav-item', index === activeAnchor && 'active')}
          aria-label={`Jump to message ${index + 1}`}
          onClick={() => {
            const container = scrollContainerRef.current;
            const element = container?.querySelector<HTMLElement>(`[data-chat-message-index="${index}"]`);
            element?.scrollIntoView({ behavior: 'smooth', block: 'start' });
          }}
        >
          <span className="astra-dot-nav-tip">{formatMessageLabel(index)}</span>
        </button>
      ))}
    </nav>
  );
}
