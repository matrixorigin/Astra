'use client';

import { Mic, SendHorizontal, Square } from 'lucide-react';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { IconButton } from '@/components/ui/icon-button';
import {
  ComposerEnvironmentChip,
  ComposerPlusMenu,
} from '@/components/app/composer-plus-menu';
import { ModelSwitcher } from '@/components/app/model-switcher';
import { SlashCommandPanel } from '@/components/app/slash-command-panel';
import type {
  AttachmentRef,
  ComposerOptions,
  EdgeStatusResponse,
  RuntimeCapabilitiesResponse,
  WorkspaceSelection,
} from '@/lib/api/types';
import { getRuntimeCapabilities } from '@/lib/api/chats';
import {
  resolveGitHubAccessAvailability,
  resolveWebAccessAvailability,
} from '@/lib/runtime-capabilities';
import { filterSlashCommands, skillToSlashCommand, type SlashCommandItem } from '@/lib/composer/slash-commands';
import { useSkillCatalog } from '@/hooks/use-skill-catalog';
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
  persistModelPreference?: boolean;
  onModelChange?: (model: string) => void;
  showStop?: boolean;
  stopping?: boolean;
  stopDisabled?: boolean;
  onStop?: () => void;
  workspaceSelection?: WorkspaceSelection | null;
  edgeWorkspaces?: EdgeStatusResponse['edges'];
  edgeWorkspacesLoading?: boolean;
  edgeWorkspacesError?: string | null;
  onWorkspaceSelectionChange?: (selection: WorkspaceSelection | null) => void;
  onRefreshEdgeWorkspaces?: () => void;
};

const SKILL_TOKEN_SELECTOR = '[data-composer-skill-token="true"]';
const COMPACT_PLACEHOLDER_MAX_CHARS = 36;

type EditorSnapshot = {
  html: string;
  text: string;
  activeSkills: string[];
};

type SlashDomQuery = {
  query: string;
  range: Range;
};

function uniqueSkillNames(skills: string[]) {
  const seen = new Set<string>();
  const result: string[] = [];
  for (const skill of skills) {
    const normalized = skill.trim();
    if (normalized && !seen.has(normalized)) {
      seen.add(normalized);
      result.push(normalized);
    }
  }
  return result;
}

export function compactComposerPlaceholder(placeholder: string) {
  const trimmed = placeholder.trim();
  if (trimmed.startsWith('Message Astra while')) {
    return 'Message Astra...';
  }
  if (trimmed.startsWith('Paused')) {
    return 'Paused...';
  }
  if (trimmed.startsWith('Task needs direction')) {
    return 'Task needs direction...';
  }
  if (trimmed.startsWith('Stopping')) {
    return 'Stopping...';
  }
  if (trimmed.startsWith('Astra is busy')) {
    return 'Astra is busy...';
  }
  if (trimmed.length <= COMPACT_PLACEHOLDER_MAX_CHARS) {
    return trimmed;
  }
  return `${trimmed.slice(0, COMPACT_PLACEHOLDER_MAX_CHARS - 3).trimEnd()}...`;
}

function createSkillToken(skillName: string) {
  const token = document.createElement('span');
  token.dataset.composerSkillToken = 'true';
  token.dataset.skillName = skillName;
  token.contentEditable = 'false';
  token.textContent = `/${skillName}`;
  token.title = 'Click to remove this skill from the turn';
  token.className = 'mx-0.5 inline-flex max-w-full cursor-pointer select-none rounded px-0.5 text-accent hover:bg-accent/10';
  return token;
}

function readEditor(editor: HTMLElement) {
  const skills: string[] = [];

  function readNode(node: Node): string {
    if (node.nodeType === Node.TEXT_NODE) {
      return node.textContent ?? '';
    }
    if (!(node instanceof HTMLElement)) {
      return '';
    }
    if (node.matches(SKILL_TOKEN_SELECTOR)) {
      const skillName = node.dataset.skillName?.trim();
      if (!skillName) {
        return '';
      }
      skills.push(skillName);
      return `/${skillName}`;
    }
    if (node.tagName === 'BR') {
      return '\n';
    }
    let text = '';
    node.childNodes.forEach((child) => {
      text += readNode(child);
    });
    if (node !== editor && (node.tagName === 'DIV' || node.tagName === 'P')) {
      text += '\n';
    }
    return text;
  }

  let text = '';
  editor.childNodes.forEach((child) => {
    text += readNode(child);
  });

  return {
    text: text.replace(/\u00a0/g, ' '),
    activeSkills: uniqueSkillNames(skills),
  };
}

function isRangeInside(range: Range, root: HTMLElement) {
  return root.contains(range.commonAncestorContainer);
}

function rangeAtEditorEnd(editor: HTMLElement) {
  const range = document.createRange();
  range.selectNodeContents(editor);
  range.collapse(false);
  return range;
}

function placeCaretAfter(node: Node) {
  const selection = window.getSelection();
  if (!selection) {
    return;
  }
  const range = document.createRange();
  range.setStartAfter(node);
  range.collapse(true);
  selection.removeAllRanges();
  selection.addRange(range);
}

function findSlashQueryAtSelection(editor: HTMLElement): SlashDomQuery | null {
  const selection = window.getSelection();
  if (!selection || selection.rangeCount === 0) {
    return null;
  }
  const caret = selection.getRangeAt(0);
  if (!caret.collapsed || !isRangeInside(caret, editor) || caret.startContainer.nodeType !== Node.TEXT_NODE) {
    return null;
  }

  const textNode = caret.startContainer;
  const text = textNode.textContent ?? '';
  const beforeCaret = text.slice(0, caret.startOffset);
  const slashIndex = beforeCaret.lastIndexOf('/');
  if (slashIndex < 0) {
    return null;
  }
  if (slashIndex > 0 && !/\s/.test(beforeCaret[slashIndex - 1] ?? '')) {
    return null;
  }

  const query = beforeCaret.slice(slashIndex + 1);
  if (/\s/.test(query)) {
    return null;
  }

  const range = document.createRange();
  range.setStart(textNode, slashIndex);
  range.setEnd(textNode, caret.startOffset);
  return { query, range };
}

export function Composer({
  placeholder = 'How can I help you today?',
  initialValue = '',
  onSubmit,
  projectContext,
  maxLength = 100_000,
  className,
  disabled,
  initialModel,
  persistModelPreference = true,
  onModelChange,
  showStop = false,
  stopping = false,
  stopDisabled = false,
  onStop,
  workspaceSelection,
  edgeWorkspaces = [],
  edgeWorkspacesLoading = false,
  edgeWorkspacesError = null,
  onWorkspaceSelectionChange,
  onRefreshEdgeWorkspaces,
}: ComposerProps) {
  const [text, setText] = useState(initialValue);
  const [webSearch, setWebSearch] = useState(false);
  const [runtimeCapabilities, setRuntimeCapabilities] =
    useState<RuntimeCapabilitiesResponse | null>(null);
  const [thinking, setThinking] = useState(true);
  const [model, setModel] = useState(initialModel ?? 'sonnet-4.6-adaptive');
  const [modelAvailable, setModelAvailable] = useState(false);
  const [activeSkills, setActiveSkills] = useState<string[]>([]);
  const [activeTools, setActiveTools] = useState<string[]>([]);
  const [slashQuery, setSlashQuery] = useState<string | null>(null);
  const [activeSlashIndex, setActiveSlashIndex] = useState(0);
  const {
    items: skillCatalogItems,
    loading: skillsLoading,
    error: skillsError,
    loadedAll: skillsLoadedAll,
    loadAll: loadAllSkills,
  } = useSkillCatalog({ pageSize: 250, maxItems: 5_000 });
  const [submitting, setSubmitting] = useState(false);
  const editorRef = useRef<HTMLDivElement | null>(null);
  const lastRangeRef = useRef<Range | null>(null);
  const slashRangeRef = useRef<Range | null>(null);
  const wasDisabledRef = useRef(Boolean(disabled));
  const canSubmit =
    text.trim().length > 0 && !submitting && !disabled && modelAvailable;
  const visualPlaceholder = compactComposerPlaceholder(placeholder);
  const slashCommands = useMemo(() => {
    if (slashQuery === null) {
      return [];
    }
    const selectedSkills = new Set(activeSkills);
    const commands = skillCatalogItems
      .filter((skill) => !selectedSkills.has(skill.name))
      .map(skillToSlashCommand);
    return filterSlashCommands(commands, slashQuery);
  }, [activeSkills, skillCatalogItems, slashQuery]);
  const showSlashPanel = slashQuery !== null && !disabled && !submitting;
  const edgeCapabilityRevision = (edgeWorkspaces ?? [])
    .map(
      (edge) =>
        `${edge.edge_agent_id}:${JSON.stringify(edge.capabilities ?? null)}`,
    )
    .join('|');
  const webAccess = useMemo(
    () => resolveWebAccessAvailability(runtimeCapabilities, workspaceSelection),
    [runtimeCapabilities, workspaceSelection],
  );
  const githubAccess = useMemo(
    () => resolveGitHubAccessAvailability(runtimeCapabilities, workspaceSelection),
    [runtimeCapabilities, workspaceSelection],
  );

  useEffect(() => {
    let cancelled = false;
    void getRuntimeCapabilities()
      .then((snapshot) => {
        if (!cancelled) setRuntimeCapabilities(snapshot);
      })
      .catch(() => {
        if (!cancelled) setRuntimeCapabilities({ tools: [] });
      });
    return () => {
      cancelled = true;
    };
  }, [edgeCapabilityRevision]);

  useEffect(() => {
    if (!webAccess.available) setWebSearch(false);
  }, [webAccess.available]);

  useEffect(() => {
    if (!githubAccess.available) {
      setActiveTools((tools) => tools.filter((tool) => tool !== 'github'));
    }
  }, [githubAccess.available]);

  useEffect(() => {
    const storedThinking = window.localStorage.getItem('astra.composer.thinking');
    const storedModel = persistModelPreference ? window.localStorage.getItem('astra.composer.model') : null;
    if (storedThinking !== null) {
      setThinking(storedThinking === 'true');
    }
    window.localStorage.removeItem('astra.composer.activeSkills');
    if (initialModel) {
      setModel(initialModel);
      return;
    }
    if (storedModel) {
      setModel(storedModel);
    }
  }, [initialModel, persistModelPreference]);

  useEffect(() => {
    window.localStorage.setItem('astra.composer.thinking', String(thinking));
  }, [thinking]);

  useEffect(() => {
    if (!persistModelPreference) {
      return;
    }
    window.localStorage.setItem('astra.composer.model', model);
  }, [model, persistModelPreference]);

  function handleModelChange(nextModel: string) {
    setModelAvailable(true);
    setModel(nextModel);
    onModelChange?.(nextModel);
  }

  useEffect(() => {
    const editor = editorRef.current;
    if (!editor || editor.textContent || editor.querySelector(SKILL_TOKEN_SELECTOR)) {
      return;
    }
    editor.textContent = initialValue;
    setText(initialValue);
  }, [initialValue]);

  useEffect(() => {
    const wasDisabled = wasDisabledRef.current;
    wasDisabledRef.current = Boolean(disabled);
    if (wasDisabled && !disabled && !submitting) {
      requestAnimationFrame(() => editorRef.current?.focus());
    }
  }, [disabled, submitting]);

  useEffect(() => {
    setActiveSlashIndex(0);
  }, [slashQuery]);

  useEffect(() => {
    if (!showSlashPanel || skillsLoadedAll || skillsLoading || skillsError) {
      return;
    }
    void loadAllSkills();
  }, [loadAllSkills, showSlashPanel, skillsError, skillsLoadedAll, skillsLoading]);

  const syncEditorState = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) {
      return;
    }
    const snapshot = readEditor(editor);
    setText(snapshot.text);
    setActiveSkills(snapshot.activeSkills);
  }, []);

  const refreshSlashQuery = useCallback(() => {
    const editor = editorRef.current;
    if (!editor) {
      slashRangeRef.current = null;
      setSlashQuery(null);
      return;
    }
    const query = findSlashQueryAtSelection(editor);
    slashRangeRef.current = query?.range.cloneRange() ?? null;
    setSlashQuery(query?.query ?? null);
  }, []);

  const saveSelection = useCallback(() => {
    const editor = editorRef.current;
    const selection = window.getSelection();
    if (!editor || !selection || selection.rangeCount === 0) {
      return;
    }
    const range = selection.getRangeAt(0);
    if (isRangeInside(range, editor)) {
      lastRangeRef.current = range.cloneRange();
      refreshSlashQuery();
    }
  }, [refreshSlashQuery]);

  const insertSkillAtRange = useCallback((skillName: string, explicitRange?: Range | null) => {
    const editor = editorRef.current;
    if (!editor) {
      return;
    }

    const token = createSkillToken(skillName);
    const trailingSpace = document.createTextNode(' ');
    const fragment = document.createDocumentFragment();
    fragment.append(token, trailingSpace);

    const savedRange = lastRangeRef.current;
    const range = explicitRange && isRangeInside(explicitRange, editor)
      ? explicitRange
      : savedRange && isRangeInside(savedRange, editor)
        ? savedRange
        : rangeAtEditorEnd(editor);

    range.deleteContents();
    range.insertNode(fragment);
    placeCaretAfter(trailingSpace);
    lastRangeRef.current = null;
    slashRangeRef.current = null;
    setSlashQuery(null);
    syncEditorState();
  }, [syncEditorState]);

  function selectSlashCommand(command: SlashCommandItem) {
    if (command.kind === 'skill') {
      insertSkillAtRange(command.value, slashRangeRef.current);
      return;
    }
  }

  function removeSkillTokens(skillNames: string[]) {
    const editor = editorRef.current;
    if (!editor || skillNames.length === 0) {
      return;
    }
    const remove = new Set(skillNames);
    editor.querySelectorAll<HTMLElement>(SKILL_TOKEN_SELECTOR).forEach((token) => {
      if (token.dataset.skillName && remove.has(token.dataset.skillName)) {
        if (token.nextSibling?.nodeType === Node.TEXT_NODE && token.nextSibling.textContent?.startsWith(' ')) {
          token.nextSibling.textContent = token.nextSibling.textContent.slice(1);
        }
        token.remove();
      }
    });
    syncEditorState();
  }

  function handleActiveSkillsChange(nextSkills: string[]) {
    const normalizedNext = uniqueSkillNames(nextSkills);
    const current = activeSkills;
    const removed = current.filter((skill) => !normalizedNext.includes(skill));
    const added = normalizedNext.filter((skill) => !current.includes(skill));
    removeSkillTokens(removed);
    for (const skill of added) {
      insertSkillAtRange(skill);
    }
  }

  function clearEditor() {
    const editor = editorRef.current;
    if (editor) {
      editor.replaceChildren();
    }
    setText('');
    setActiveSkills([]);
    slashRangeRef.current = null;
    lastRangeRef.current = null;
    setSlashQuery(null);
  }

  function restoreEditor(snapshot: EditorSnapshot) {
    const editor = editorRef.current;
    if (editor) {
      editor.innerHTML = snapshot.html;
    }
    setText(snapshot.text);
    setActiveSkills(snapshot.activeSkills);
  }

  function insertPlainTextAtSelection(value: string) {
    const editor = editorRef.current;
    if (!editor) {
      return;
    }
    const selection = window.getSelection();
    const range = selection?.rangeCount && isRangeInside(selection.getRangeAt(0), editor)
      ? selection.getRangeAt(0)
      : rangeAtEditorEnd(editor);
    const node = document.createTextNode(value);
    range.deleteContents();
    range.insertNode(node);
    placeCaretAfter(node);
    syncEditorState();
    refreshSlashQuery();
  }

  async function submit() {
    const trimmed = text.trim();
    if (!trimmed || submitting || disabled || !modelAvailable) {
      return;
    }
    const editor = editorRef.current;
    const snapshot: EditorSnapshot = {
      html: editor?.innerHTML ?? '',
      text,
      activeSkills,
    };
    const submittedSkills = [...activeSkills];
    const submittedTools = [
      ...new Set([
        ...activeTools,
        ...(webSearch ? ['web_search', 'web_fetch'] : []),
      ]),
    ];
    setSubmitting(true);
    clearEditor();
    try {
      await onSubmit({
        text: trimmed,
        attachments: [],
        options: {
          webSearch,
          thinking,
          model,
          activeSkills: submittedSkills,
          activeTools: submittedTools,
        },
      });
    } catch (error) {
      restoreEditor(snapshot);
      throw error;
    } finally {
      setSubmitting(false);
      requestAnimationFrame(() => editorRef.current?.focus());
    }
  }

  return (
    <form
      onSubmit={(event) => {
        event.preventDefault();
        submit();
      }}
      className={cn(
        'relative rounded-[20px] border border-border bg-surface shadow-[0_4px_20px_rgba(28,25,23,0.04),0_1px_3px_rgba(28,25,23,0.06)] transition-[border-color,box-shadow] focus-within:border-border-strong focus-within:shadow-[0_4px_24px_rgba(28,25,23,0.08),0_0_0_3px_rgba(37,99,235,0.10)]',
        className,
      )}
    >
      <div className="flex min-h-14 w-full flex-wrap items-start gap-x-1 gap-y-2 rounded-t-[20px] px-4 pb-2 pt-3 text-[17px]">
        <div
          ref={editorRef}
          data-composer-input="true"
          contentEditable={!disabled && !submitting}
          suppressHydrationWarning
          suppressContentEditableWarning
          role="textbox"
          aria-label={placeholder}
          aria-multiline="true"
          title={placeholder}
          onInput={() => {
            syncEditorState();
            refreshSlashQuery();
          }}
          onMouseUp={saveSelection}
          onKeyUp={saveSelection}
          onFocus={saveSelection}
          onPaste={(event) => {
            event.preventDefault();
            insertPlainTextAtSelection(event.clipboardData.getData('text/plain').slice(0, maxLength));
          }}
          onMouseDown={(event) => {
            const token = (event.target as HTMLElement).closest<HTMLElement>(SKILL_TOKEN_SELECTOR);
            if (!token) {
              return;
            }
            event.preventDefault();
            token.remove();
            syncEditorState();
            requestAnimationFrame(() => editorRef.current?.focus());
          }}
          onKeyDown={(event) => {
            if (showSlashPanel) {
              if (event.key === 'ArrowDown') {
                event.preventDefault();
                setActiveSlashIndex((index) => Math.min(index + 1, Math.max(slashCommands.length - 1, 0)));
                return;
              }
              if (event.key === 'ArrowUp') {
                event.preventDefault();
                setActiveSlashIndex((index) => Math.max(index - 1, 0));
                return;
              }
              if (event.key === 'Escape') {
                event.preventDefault();
                setSlashQuery(null);
                return;
              }
              if (event.key === 'Enter') {
                event.preventDefault();
                if (slashCommands[activeSlashIndex]) {
                  selectSlashCommand(slashCommands[activeSlashIndex]);
                }
                return;
              }
            }
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              submit();
            }
          }}
          className="max-h-[200px] min-h-7 min-w-[14rem] flex-1 overflow-y-auto whitespace-pre-wrap break-words border-0 bg-transparent p-0 text-[17px] leading-[1.6] text-text shadow-none outline-none ring-0 empty:before:text-text-muted empty:before:content-[attr(data-placeholder)] focus:border-0 focus:outline-none focus:ring-0 focus-visible:outline-none focus-visible:ring-0"
          data-placeholder={visualPlaceholder}
        />
      </div>
      {showSlashPanel ? (
        <SlashCommandPanel
          items={slashCommands}
          loading={skillsLoading}
          error={skillsError}
          activeIndex={activeSlashIndex}
          onSelect={selectSlashCommand}
        />
      ) : null}
      <div className="flex min-h-11 items-center gap-2 px-3 pb-2 pt-1">
        <ComposerPlusMenu
          inProject={Boolean(projectContext)}
          webSearch={webSearch}
          webAccess={webAccess}
          githubAccess={githubAccess}
          onWebSearchChange={setWebSearch}
          activeSkills={activeSkills}
          onActiveSkillsChange={handleActiveSkillsChange}
          activeTools={activeTools}
          onActiveToolsChange={setActiveTools}
          workspaceSelection={workspaceSelection}
          edgeWorkspaces={edgeWorkspaces}
          edgeWorkspacesLoading={edgeWorkspacesLoading}
          edgeWorkspacesError={edgeWorkspacesError}
          onWorkspaceSelectionChange={onWorkspaceSelectionChange}
          onRefreshEdgeWorkspaces={onRefreshEdgeWorkspaces}
        />
        <ComposerEnvironmentChip
          workspaceSelection={workspaceSelection}
          edgeWorkspaces={edgeWorkspaces}
          edgeWorkspacesLoading={edgeWorkspacesLoading}
          edgeWorkspacesError={edgeWorkspacesError}
          onWorkspaceSelectionChange={onWorkspaceSelectionChange}
          onRefreshEdgeWorkspaces={onRefreshEdgeWorkspaces}
        />
        <IconButton icon={Mic} label="Voice input" tooltip="Coming soon" disabled />
        <span
          className={cn(
            'ml-auto size-2 rounded-full',
            submitting ? 'animate-pulse bg-warning' : disabled ? 'bg-border-strong' : 'bg-success',
          )}
          aria-label={submitting ? 'Streaming' : 'Ready'}
        />
        <ModelSwitcher
          value={model}
          onChange={handleModelChange}
          onModelAvailabilityChange={setModelAvailable}
          thinking={thinking}
          onThinkingChange={setThinking}
        />
        {showStop ? (
          <IconButton
            icon={Square}
            label={stopping ? 'Stopping run' : 'Stop run'}
            type="button"
            disabled={stopDisabled}
            onClick={onStop}
            className="rounded-full border border-border bg-bg text-text hover:bg-surface-muted"
          />
        ) : null}
        <IconButton
          icon={SendHorizontal}
          label="Send message"
          type="submit"
          disabled={!canSubmit}
          className={cn(
            canSubmit
              ? 'rounded-full bg-text text-white hover:bg-black hover:text-white'
              : 'rounded-full bg-border text-white',
          )}
        />
      </div>
    </form>
  );
}
