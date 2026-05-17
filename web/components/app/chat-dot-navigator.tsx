'use client';

import type { CSSProperties, RefObject } from 'react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { cn } from '@/lib/utils/cn';
import type { ChatMessage } from '@/lib/api/types';

type ChatDotNavigatorProps = {
  messages: Pick<ChatMessage, 'role' | 'content'>[];
  scrollContainerRef: RefObject<HTMLDivElement | null>;
};

const FIRST_NAVIGABLE_MESSAGE_COUNT = 1;
const FULL_ANCHOR_MESSAGE_LIMIT = 6;
const USER_INPUT_PREVIEW_LIMIT = 80;

function computeStep(total: number) {
  if (total <= FULL_ANCHOR_MESSAGE_LIMIT) {
    return 1;
  }
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

function formatMessageNumber(index: number) {
  const number = index + 1;
  if (number >= 1000) {
    return `${(number / 1000).toFixed(number % 1000 === 0 ? 0 : 1)}k`;
  }
  return `Msg ${number}`;
}

function compactUserInput(input: string) {
  const compact = input.replace(/\s+/g, ' ').trim();
  if (compact.length <= USER_INPUT_PREVIEW_LIMIT) {
    return compact;
  }
  const preview = compact.slice(0, USER_INPUT_PREVIEW_LIMIT - 3);
  const lastSpace = preview.lastIndexOf(' ');
  const minWordBoundary = Math.floor((USER_INPUT_PREVIEW_LIMIT - 3) * 0.6);
  const prefix = lastSpace >= minWordBoundary ? preview.slice(0, lastSpace) : preview.trimEnd();
  return `${prefix}...`;
}

function userInputForAnchor(messages: Pick<ChatMessage, 'role' | 'content'>[], index: number) {
  for (let cursor = Math.min(index, messages.length - 1); cursor >= 0; cursor -= 1) {
    const message = messages[cursor];
    if (message?.role === 'user') {
      const input = compactUserInput(message.content);
      if (input) {
        return input;
      }
    }
  }
  return '';
}

function formatMessageLabel(messages: Pick<ChatMessage, 'role' | 'content'>[], index: number) {
  const number = formatMessageNumber(index);
  const input = userInputForAnchor(messages, index);
  return input ? `${number}: ${input}` : number;
}

function scrollToMessage(container: HTMLDivElement, element: HTMLElement, index: number, messageCount: number) {
  const maxScroll = Math.max(container.scrollHeight - container.clientHeight, 0);
  if (index === messageCount - 1) {
    container.scrollTo({ top: maxScroll, behavior: 'smooth' });
    return;
  }

  const containerTop = container.getBoundingClientRect().top;
  const elementTop = element.getBoundingClientRect().top;
  const targetTop = elementTop - containerTop + container.scrollTop;
  container.scrollTo({ top: Math.max(0, Math.min(targetTop, maxScroll)), behavior: 'smooth' });
}

export function ChatDotNavigator({ messages, scrollContainerRef }: ChatDotNavigatorProps) {
  const messageCount = messages.length;
  const [activeIndex, setActiveIndex] = useState(0);
  const [progress, setProgress] = useState(0);
  const anchors = useMemo(() => {
    if (messageCount < FIRST_NAVIGABLE_MESSAGE_COUNT) {
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
      {anchors.map((index) => {
        const label = formatMessageLabel(messages, index);
        return (
          <button
            key={index}
            type="button"
            className={cn('astra-dot-nav-item', index === activeAnchor && 'active')}
            aria-label={`Jump to ${label}`}
            onClick={() => {
              const container = scrollContainerRef.current;
              const element = container?.querySelector<HTMLElement>(`[data-chat-message-index="${index}"]`);
              if (container && element) {
                scrollToMessage(container, element, index, messageCount);
              }
            }}
          >
            <span className="astra-dot-nav-tip">{label}</span>
          </button>
        );
      })}
    </nav>
  );
}
