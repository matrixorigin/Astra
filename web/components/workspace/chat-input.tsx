'use client';

import { useState, useCallback, type KeyboardEvent } from 'react';

export function ChatInput({
  onSend,
  onStop,
  disabled = false,
  isStreaming = false,
  placeholder = 'Send a message…',
}: {
  onSend: (message: string) => void;
  onStop?: () => void;
  disabled?: boolean;
  isStreaming?: boolean;
  placeholder?: string;
}) {
  const [value, setValue] = useState('');

  const handleSend = useCallback(() => {
    const trimmed = value.trim();
    if (trimmed.length === 0 || disabled) return;
    onSend(trimmed);
    setValue('');
  }, [value, disabled, onSend]);

  const handleKeyDown = useCallback(
    (e: KeyboardEvent<HTMLTextAreaElement>) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        handleSend();
      }
    },
    [handleSend],
  );

  return (
    <div className="border-t border-slate-800 bg-slate-950/80 p-4">
      <div className="flex gap-3">
        <textarea
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={handleKeyDown}
          disabled={disabled}
          placeholder={placeholder}
          rows={1}
          className="flex-1 resize-none rounded-2xl border border-slate-700 bg-slate-900/50 px-4 py-3 text-sm text-white outline-none placeholder:text-slate-500 focus:border-sky-500/50 disabled:opacity-50"
        />
        {isStreaming && onStop ? (
          <button
            type="button"
            onClick={onStop}
            className="rounded-2xl bg-red-600 px-5 py-3 text-sm font-medium text-white hover:bg-red-500"
          >
            Stop
          </button>
        ) : (
          <button
            type="button"
            onClick={handleSend}
            disabled={disabled || value.trim().length === 0}
            className="rounded-2xl bg-sky-600 px-5 py-3 text-sm font-medium text-white hover:bg-sky-500 disabled:opacity-40"
          >
            Send
          </button>
        )}
      </div>
      <p className="mt-2 text-xs text-slate-500">
        Press Enter to send, Shift+Enter for new line
      </p>
    </div>
  );
}
