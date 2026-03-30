'use client';

import { useRef, useEffect } from 'react';
import type { ChatMessage } from '@/lib/workspace/types';

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
        {message.role === 'assistant' && message.streaming && message.content === '' ? (
          <span className="inline-block animate-pulse text-slate-400">Thinking…</span>
        ) : (
          <div className="whitespace-pre-wrap break-words">{message.content}</div>
        )}

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

        {message.streaming ? (
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

  useEffect(() => {
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages]);

  return (
    <div ref={scrollRef} className={`flex-1 overflow-y-auto p-4 ${className}`}>
      {messages.length === 0 ? (
        <div className="flex h-full items-center justify-center">
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
