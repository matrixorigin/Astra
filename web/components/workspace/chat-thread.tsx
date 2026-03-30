'use client';

import { useRef, useEffect, useCallback } from 'react';
import type { ChatMessage } from '@/lib/workspace/types';
import { MarkdownRenderer } from './markdown-renderer';
import { ThinkingBlock } from './thinking-block';

function MessageBubble({ message }: { message: ChatMessage }) {
  const isUser = message.role === 'user';

  return (
    <div className={`flex ${isUser ? 'justify-end' : 'justify-start'}`}>
      <div
        className={`max-w-[85%] rounded-2xl px-4 py-3 text-sm leading-relaxed ${
          isUser
            ? 'bg-sky-600/20 text-sky-100'
            : 'border border-slate-800 bg-slate-950/70 text-slate-200'
        }`}
      >
        {/* Thinking block (assistant only) */}
        {message.thinking && <ThinkingBlock thinking={message.thinking} />}

        {/* Content */}
        {message.role === 'assistant' && message.streaming && message.content === '' ? (
          <span className="inline-block animate-pulse text-slate-400">
            {message.thinking ? '' : 'Thinking…'}
          </span>
        ) : isUser ? (
          <div className="whitespace-pre-wrap break-words">{message.content}</div>
        ) : (
          <MarkdownRenderer content={message.content} />
        )}

        {/* Inline tool call summary */}
        {message.toolCalls && message.toolCalls.length > 0 ? (
          <div className="mt-2 space-y-1 border-t border-slate-700/50 pt-2">
            {message.toolCalls.map((tc) => (
              <div key={tc.callId} className="flex items-center gap-2 text-xs text-slate-400">
                <span
                  className={`inline-block h-1.5 w-1.5 rounded-full ${
                    tc.status === 'running'
                      ? 'animate-pulse bg-amber-400'
                      : tc.status === 'done'
                        ? 'bg-emerald-400'
                        : 'bg-red-400'
                  }`}
                />
                <span className="font-mono">{tc.tool}</span>
              </div>
            ))}
          </div>
        ) : null}

        {/* Streaming cursor */}
        {message.streaming && message.content !== '' ? (
          <span className="ml-1 inline-block h-3 w-1 animate-pulse bg-sky-400/60" />
        ) : null}
      </div>
    </div>
  );
}

export function ChatThread({
  messages,
  className = '',
}: {
  messages: ChatMessage[];
  className?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const isNearBottomRef = useRef(true);

  // Track whether user is near the bottom (within 100px)
  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    isNearBottomRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 100;
  }, []);

  // Only auto-scroll if user hasn't scrolled up
  useEffect(() => {
    const el = scrollRef.current;
    if (el && isNearBottomRef.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages]);

  return (
    <div
      ref={scrollRef}
      onScroll={handleScroll}
      className={`flex-1 overflow-y-auto p-4 ${className}`}
    >
      {messages.length === 0 ? (
        <div className="flex h-full flex-col items-center justify-center gap-3">
          <div className="flex h-12 w-12 items-center justify-center rounded-full bg-slate-800/50">
            <svg className="h-6 w-6 text-slate-500" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={1.5} d="M8 12h.01M12 12h.01M16 12h.01M21 12c0 4.418-4.03 8-9 8a9.863 9.863 0 01-4.255-.949L3 20l1.395-3.72C3.512 15.042 3 13.574 3 12c0-4.418 4.03-8 9-8s9 3.582 9 8z" />
            </svg>
          </div>
          <p className="text-sm text-slate-500">
            Send a message to start the conversation.
          </p>
        </div>
      ) : (
        <div className="space-y-4">
          {messages.map((message) => (
            <MessageBubble key={message.id} message={message} />
          ))}
        </div>
      )}
    </div>
  );
}
