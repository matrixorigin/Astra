'use client';

import { useState, useCallback, type KeyboardEvent } from 'react';

export function ChatInput({
  onSend,
  onStop,
  disabled = false,
  isStreaming = false,
  placeholder = 'Send a message…',
  followupSuggestion = null,
}: {
  onSend: (message: string) => void;
  onStop?: () => void;
  disabled?: boolean;
  isStreaming?: boolean;
  placeholder?: string;
  followupSuggestion?: string | null;
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
      if (e.key === 'Tab' && !e.shiftKey && value.length === 0 && followupSuggestion) {
        e.preventDefault();
        setValue(followupSuggestion);
        return;
      }
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        handleSend();
      }
    },
    [followupSuggestion, handleSend, value.length],
  );

  return (
    <div className="border-t border-slate-800 bg-slate-950/80 p-4">
      {followupSuggestion && value.length === 0 && !isStreaming ? (
        <div className="mb-3 flex items-center gap-2 text-xs">
          <button
            type="button"
            onClick={() => setValue(followupSuggestion)}
            className="rounded-full border border-sky-500/40 bg-sky-500/10 px-3 py-1 text-sky-200 hover:bg-sky-500/20"
          >
            Next: {followupSuggestion}
          </button>
          <span className="text-slate-500">Press Tab to accept</span>
        </div>
      ) : null}
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
