'use client';

import { Mic, SendHorizontal, X } from 'lucide-react';
import { useEffect, useRef, useState } from 'react';
import { IconButton } from '@/components/ui/icon-button';
import { ComposerPlusMenu } from '@/components/app/composer-plus-menu';
import { ModelSwitcher } from '@/components/app/model-switcher';
import type { AttachmentRef, ComposerOptions } from '@/lib/api/types';
import { cn } from '@/lib/utils/cn';

type ComposerProps = {
  placeholder?: string;
  initialValue?: string;
  onSubmit: (payload: {
    text: string;
    attachments: AttachmentRef[];
    options: ComposerOptions;
  }) => Promise<void>;
  projectContext?: { projectId: string };
  maxLength?: number;
  className?: string;
  disabled?: boolean;
  initialModel?: string;
};

export function Composer({
  placeholder = 'How can I help you today?',
  initialValue = '',
  onSubmit,
  projectContext,
  maxLength = 100_000,
  className,
  disabled,
  initialModel,
}: ComposerProps) {
  const [text, setText] = useState(initialValue);
  const [webSearch, setWebSearch] = useState(false);
  const [thinking, setThinking] = useState(true);
  const [model, setModel] = useState(initialModel ?? 'sonnet-4.6-adaptive');
  const [activeSkills, setActiveSkills] = useState<string[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const wasDisabledRef = useRef(Boolean(disabled));
  const canSubmit = text.trim().length > 0 && !submitting && !disabled;

  useEffect(() => {
    const storedThinking = window.localStorage.getItem('astra.composer.thinking');
    const storedModel = window.localStorage.getItem('astra.composer.model');
    const storedSkills = window.localStorage.getItem('astra.composer.activeSkills');
    if (storedThinking !== null) {
      setThinking(storedThinking === 'true');
    }
    if (storedSkills) {
      try {
        const parsed = JSON.parse(storedSkills) as unknown;
        if (Array.isArray(parsed)) {
          setActiveSkills(parsed.filter((skill): skill is string => typeof skill === 'string'));
        }
      } catch {
        window.localStorage.removeItem('astra.composer.activeSkills');
      }
    }
    if (initialModel) {
      setModel(initialModel);
      return;
    }
    if (storedModel) {
      setModel(storedModel);
    }
  }, [initialModel]);

  useEffect(() => {
    window.localStorage.setItem('astra.composer.thinking', String(thinking));
  }, [thinking]);

  useEffect(() => {
    window.localStorage.setItem('astra.composer.model', model);
  }, [model]);

  useEffect(() => {
    window.localStorage.setItem('astra.composer.activeSkills', JSON.stringify(activeSkills));
  }, [activeSkills]);

  useEffect(() => {
    const area = textareaRef.current;
    if (!area) {
      return;
    }
    area.style.height = '0px';
    area.style.height = `${Math.min(area.scrollHeight, 240)}px`;
  }, [text]);

  useEffect(() => {
    const wasDisabled = wasDisabledRef.current;
    wasDisabledRef.current = Boolean(disabled);
    if (wasDisabled && !disabled && !submitting) {
      requestAnimationFrame(() => textareaRef.current?.focus());
    }
  }, [disabled, submitting]);

  async function submit() {
    const trimmed = text.trim();
    if (!trimmed || submitting || disabled) {
      return;
    }
    setSubmitting(true);
    setText('');
    try {
      await onSubmit({
        text: trimmed,
        attachments: [],
        options: { webSearch, thinking, model, activeSkills },
      });
    } catch (error) {
      setText(trimmed);
      throw error;
    } finally {
      setSubmitting(false);
      requestAnimationFrame(() => textareaRef.current?.focus());
    }
  }

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
      className={cn(
        'rounded-[20px] border border-transparent bg-surface shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.06),0_0_0_0.5px_rgba(120,113,108,0.22)] transition-shadow hover:shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.08),0_0_0_0.5px_rgba(120,113,108,0.32)] focus-within:shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.12),0_0_0_1px_rgb(var(--color-accent))]',
        className,
      )}
    >
      <textarea
        ref={textareaRef}
        data-composer-input="true"
        value={text}
        maxLength={maxLength}
        disabled={disabled || submitting}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter' && !event.shiftKey) {
            event.preventDefault();
            submit();
          }
        }}
        placeholder={placeholder}
        className="max-h-60 min-h-20 w-full resize-none rounded-t-[20px] bg-transparent px-5 pb-3 pt-4 text-base text-text outline-none placeholder:text-text-muted disabled:opacity-60"
      />
      {activeSkills.length ? (
        <div className="flex flex-wrap gap-1 px-4 pb-2">
          {activeSkills.map((skill) => (
            <button
              key={skill}
              type="button"
              disabled={disabled || submitting}
              onClick={() => setActiveSkills((current) => current.filter((item) => item !== skill))}
              className="inline-flex h-7 max-w-full items-center gap-1 rounded-full bg-surface-muted px-2 text-xs text-text-secondary hover:bg-border disabled:cursor-not-allowed disabled:opacity-50"
            >
              <span className="truncate">{skill}</span>
              <X className="size-3" />
            </button>
          ))}
        </div>
      ) : null}
      <div className="flex min-h-12 items-center gap-2 px-3 pb-3 pt-1">
        <ComposerPlusMenu
          inProject={Boolean(projectContext)}
          webSearch={webSearch}
          onWebSearchChange={setWebSearch}
          activeSkills={activeSkills}
          onActiveSkillsChange={setActiveSkills}
        />
        <IconButton icon={Mic} label="Voice input" tooltip="Coming soon" disabled />
        <span
          className={cn(
            'ml-auto size-2 rounded-full',
            submitting ? 'animate-pulse bg-warning' : disabled ? 'bg-border-strong' : 'bg-success',
          )}
          aria-label={submitting ? 'Streaming' : 'Ready'}
        />
        <ModelSwitcher value={model} onChange={setModel} thinking={thinking} onThinkingChange={setThinking} />
        <IconButton
          icon={SendHorizontal}
          label="Send message"
          type="submit"
          disabled={!canSubmit}
          className={cn(
            canSubmit
              ? 'bg-accent text-white hover:bg-accent/90 hover:text-white'
              : 'bg-border-strong text-white',
          )}
        />
      </div>
    </form>
  );
}
