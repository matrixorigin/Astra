'use client';

import { Bot, CheckCircle2, ChevronDown, Clock3, Copy, Loader, RefreshCcw, ThumbsDown, ThumbsUp, User } from 'lucide-react';
import { Children, isValidElement, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import type { Components } from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeKatex from 'rehype-katex';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import { IconButton } from '@/components/ui/icon-button';
import { splitThinkingTags } from '@/lib/api/chats';
import type { ChatMessage } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

const markdownRemarkPlugins = [remarkGfm, remarkMath];
const markdownRehypePlugins = [rehypeKatex, rehypeHighlight];

const markdownComponents: Components = {
  pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
  p: ({ children }) => {
    const elementChildren = Children.toArray(children).filter((child) => (
      typeof child !== 'string' || child.trim().length > 0
    ));
    const mathOnly = elementChildren.length === 1 && isKatexElement(elementChildren[0]);
    return <p className={mathOnly ? 'text-center' : undefined}>{children}</p>;
  },
  table: ({ children }) => (
    <div className="mb-6 w-full overflow-x-auto px-2">
      <table>{children}</table>
    </div>
  ),
};

function CodeBlock({ children }: { children: ReactNode }) {
  const [copied, setCopied] = useState(false);
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const codeText = extractText(children).replace(/\n$/, '');

  useEffect(() => () => {
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current);
    }
  }, []);

  const copyCode = async () => {
    try {
      await navigator.clipboard.writeText(codeText);
      setCopied(true);
      if (timeoutRef.current) {
        clearTimeout(timeoutRef.current);
      }
      timeoutRef.current = setTimeout(() => setCopied(false), 1400);
    } catch (error) {
      console.warn('Failed to copy code block', error);
    }
  };

  return (
    <div className="group/code relative">
      <button
        type="button"
        onClick={copyCode}
        className="absolute right-2 top-2 z-10 inline-flex items-center gap-1 rounded-md border border-white/10 bg-[#161b22]/90 px-2 py-1 text-xs font-medium text-[#c9d1d9] opacity-80 shadow-sm backdrop-blur transition hover:bg-[#21262d] hover:text-white hover:opacity-100 focus:opacity-100 focus:outline-none"
        aria-label={copied ? 'Code copied' : 'Copy code'}
      >
        {copied ? <CheckCircle2 className="size-3.5" /> : <Copy className="size-3.5" />}
        <span>{copied ? 'Copied' : 'Copy'}</span>
      </button>
      <pre>{children}</pre>
    </div>
  );
}

function extractText(node: ReactNode): string {
  if (typeof node === 'string' || typeof node === 'number') {
    return String(node);
  }
  if (Array.isArray(node)) {
    return node.map(extractText).join('');
  }
  if (isValidElement<{ children?: ReactNode }>(node)) {
    return extractText(node.props.children);
  }
  return '';
}

function isKatexElement(child: ReactNode) {
  if (!isValidElement<{ className?: string | string[] }>(child)) {
    return false;
  }
  const className = child.props.className;
  if (Array.isArray(className)) {
    return className.includes('katex');
  }
  return typeof className === 'string' && className.split(/\s+/).includes('katex');
}

export function MessageBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === 'user';
  const splitContent = splitThinkingTags(message.content);
  const rawContent = splitContent.visibleText;
  const rawReasoning = message.reasoning?.trim() || splitContent.reasoning;
  const orphanStreamingReasoning = !isUser
    && message.status === 'streaming'
    && !rawReasoning.trim()
    && !splitContent.hasThinking
    && isLikelyOrphanStreamingReasoning(rawContent);
  const content = orphanStreamingReasoning ? '' : rawContent;
  const reasoning = orphanStreamingReasoning ? rawContent : rawReasoning;
  const reasoningStreaming = message.reasoningStatus === 'streaming' || splitContent.reasoningOpen || orphanStreamingReasoning;
  const hasReasoning = Boolean(reasoning.trim());
  const showReasoning = !isUser && (
    hasReasoning ||
    message.reasoningStatus === 'streaming' ||
    (message.status === 'streaming' && reasoningStreaming)
  );
  const isStreamingEmpty = message.status === 'streaming' && !content.trim() && !hasReasoning;
  return (
    <article className="flex gap-4 py-5">
      <div className="mt-1 flex size-8 shrink-0 items-center justify-center rounded-full bg-surface-muted">
        {isUser ? <User className="size-4" /> : <Bot className="size-4" />}
      </div>
      <div className="min-w-0 flex-1">
        <div className="mb-2 flex items-center gap-2">
          <span className="text-sm font-semibold">{isUser ? 'You' : 'Astra'}</span>
          {message.status === 'failed' ? (
            <span className="rounded-full bg-danger/10 px-2 py-0.5 text-xs text-danger">offline preview</span>
          ) : null}
        </div>
        {showReasoning ? (
          <ReasoningPanel
            reasoning={reasoning}
            streaming={reasoningStreaming}
          />
        ) : null}
        {isStreamingEmpty ? (
          <div className="flex h-7 items-center gap-1 text-text-muted" aria-label="Astra is responding">
            <span className="size-1.5 animate-pulse rounded-full bg-text-muted" />
            <span className="size-1.5 animate-pulse rounded-full bg-text-muted [animation-delay:120ms]" />
            <span className="size-1.5 animate-pulse rounded-full bg-text-muted [animation-delay:240ms]" />
          </div>
        ) : (
          <MarkdownContent content={content} />
        )}
        {!isUser && message.status !== 'streaming' ? (
          <div className="mt-3 flex gap-1">
            <IconButton icon={Copy} label="Copy response" onClick={() => navigator.clipboard?.writeText(content)} />
            <IconButton icon={RefreshCcw} label="Regenerate response" disabled />
            <IconButton icon={ThumbsUp} label="Good response" />
            <IconButton icon={ThumbsDown} label="Bad response" />
          </div>
        ) : null}
      </div>
    </article>
  );
}

function ReasoningPanel({ reasoning, streaming }: { reasoning: string; streaming: boolean }) {
  const [open, setOpen] = useState(streaming);
  const [expandedItems, setExpandedItems] = useState<Record<number, boolean>>({});
  const userToggledRef = useRef(false);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const items = reasoningItems(reasoning);
  const visibleItems = items.length > 0
    ? items
    : [streaming ? 'Preparing response...' : 'Done'];
  const summary = streaming ? 'Thinking' : firstLine(reasoning);

  useEffect(() => {
    if (userToggledRef.current) {
      return;
    }
    setOpen(streaming);
  }, [streaming]);

  useEffect(() => {
    if (open && streaming) {
      bodyRef.current?.scrollTo({ top: bodyRef.current.scrollHeight });
    }
  }, [open, reasoning, streaming]);

  return (
    <div className="mb-4 max-w-3xl pl-2 py-1.5">
      <button
        type="button"
        onClick={() => {
          userToggledRef.current = true;
          setOpen((value) => !value);
        }}
        aria-expanded={open}
        className="group/status flex w-full min-w-0 items-center gap-2 py-1 text-left text-sm text-text-muted transition-colors hover:text-text-secondary"
      >
        <span className="inline-flex min-w-0 items-center gap-1">
          {streaming ? <Loader className="size-3.5 shrink-0 animate-spin text-warning" /> : null}
          <span className="truncate text-sm font-normal">{summary}</span>
          <ChevronDown className={cn('size-3 shrink-0 transition-transform duration-200', open && 'rotate-180')} />
        </span>
      </button>
      <span className="sr-only" role="status" aria-live="polite">
        {streaming ? 'Astra is thinking' : 'Astra finished thinking'}
      </span>
      <div
        className={cn(
          'grid transition-[grid-template-rows] duration-300 ease-out',
          open ? 'grid-rows-[1fr]' : 'grid-rows-[0fr]',
        )}
      >
        <div className="min-w-0 overflow-hidden">
          <div
            ref={bodyRef}
            className={cn(
              'min-w-0 pr-1',
              open && (streaming ? 'max-h-48 overflow-y-auto' : 'max-h-64 overflow-y-auto'),
            )}
          >
            {visibleItems.map((item, index) => {
              const expanded = Boolean(expandedItems[index]);
              const isLong = isLongReasoningItem(item);
              return (
                <div key={`${index}-${item.slice(0, 24)}`} className="min-w-0">
                  <div className="flex h-2 flex-row">
                    <div className="flex w-5 justify-center">
                      {index > 0 ? <div className="h-full w-px bg-border" /> : null}
                    </div>
                  </div>
                  <div className="flex flex-row">
                    <div className="flex w-5 shrink-0 justify-center">
                      <div className="flex flex-col items-center pt-1">
                        <Clock3 className="size-4 text-text-muted" />
                        <div className="mt-1 w-px flex-1 bg-border" />
                      </div>
                    </div>
                    <div className="min-w-0 flex-1 pt-0.5">
                      <div className="px-2.5 text-text-secondary">
                        <div
                          className={cn(
                            'relative overflow-hidden transition-[max-height] duration-300 ease-out',
                            expanded || !isLong ? 'max-h-none' : 'max-h-[200px]',
                          )}
                        >
                          <MarkdownContent content={item} muted />
                          {!expanded && isLong ? (
                            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-10 bg-gradient-to-t from-bg to-transparent" />
                          ) : null}
                        </div>
                        {isLong ? (
                          <button
                            type="button"
                            onClick={() => setExpandedItems((current) => ({ ...current, [index]: !expanded }))}
                            className="mt-1 text-xs text-text-muted/80 transition-colors hover:text-text"
                          >
                            {expanded ? 'Show less' : 'Show more'}
                          </button>
                        ) : null}
                      </div>
                    </div>
                  </div>
                </div>
              );
            })}
            {!streaming ? (
              <>
                <div className="flex h-2 flex-row">
                  <div className="flex w-5 justify-center">
                    <div className="h-full w-px bg-border" />
                  </div>
                </div>
                <div className="flex flex-row">
                  <div className="flex w-5 shrink-0 justify-center">
                    <div className="flex flex-col items-center pt-0.5">
                      <CheckCircle2 className="size-4 text-text-muted" />
                    </div>
                  </div>
                  <div className="min-w-0 flex-1 pl-2.5 pt-0.5 text-[15px] leading-6 text-text-secondary">
                    Done
                  </div>
                </div>
              </>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}

function MarkdownContent({ content, muted }: { content: string; muted?: boolean }) {
  return (
    <div className={cn('astra-markdown', muted && 'text-text-secondary [&_*]:text-inherit')}>
      <ReactMarkdown
        remarkPlugins={markdownRemarkPlugins}
        rehypePlugins={markdownRehypePlugins}
        components={markdownComponents}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
}

function isLikelyOrphanStreamingReasoning(text: string) {
  const value = text.trim();
  if (value.length < 12) {
    return false;
  }
  return /^(The user\b|User\b|They(?:'re| are)\b|This is\b|I (?:need|should|will|can|must|want|have to|am going)\b|We need\b|Need to\b|Let me\b|Let's\b)/i.test(value);
}

function firstLine(text: string) {
  const line = text.trim().split(/\r?\n/).find(Boolean);
  if (!line) {
    return 'Done';
  }
  return line.length > 56 ? `${line.slice(0, 53)}...` : line;
}

function reasoningItems(text: string) {
  const normalized = text.trim().replace(/\n{3,}/g, '\n\n');
  if (!normalized) {
    return [];
  }
  const blocks = normalized.split(/\n{2,}/).map((block) => block.trim()).filter(Boolean);
  if (blocks.length <= 1) {
    return [normalized];
  }

  const items: string[] = [];
  let current = '';
  for (const block of blocks) {
    const next = current ? `${current}\n\n${block}` : block;
    if (next.length <= 720) {
      current = next;
      continue;
    }
    if (current) {
      items.push(current);
    }
    current = block;
  }
  if (current) {
    items.push(current);
  }
  return items;
}

function isLongReasoningItem(text: string) {
  const lineCount = text.split(/\r?\n/).length;
  return text.length > 520 || lineCount > 8;
}
