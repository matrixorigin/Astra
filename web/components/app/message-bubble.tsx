'use client';

import { CheckCircle2, ChevronDown, Clock3, Copy, Download, Loader, RefreshCcw, ThumbsDown, ThumbsUp } from 'lucide-react';
import { Children, isValidElement, useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import ReactMarkdown from 'react-markdown';
import type { Components } from 'react-markdown';
import rehypeHighlight from 'rehype-highlight';
import rehypeKatex from 'rehype-katex';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import { SkillMentionText } from '@/components/app/skill-mention-text';
import { IconButton } from '@/components/ui/icon-button';
import { splitThinkingTags } from '@/lib/api/chats';
import type { ChatArtifactRef, ChatMessage } from '@/lib/api/types';
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
  const language = extractCodeLanguage(children);

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
    <div className="my-4 overflow-hidden rounded-[10px] border border-[#1e2329] bg-[#0f1419]">
      <div className="flex items-center border-b border-[#1e2329] bg-white/[0.03] px-3.5 py-2 font-mono text-[11px] font-medium tracking-[0.02em] text-[#6e7681]">
        <span className="mr-3 flex gap-1.5" aria-hidden="true">
          <span className="size-2.5 rounded-full bg-[#ff5f56]" />
          <span className="size-2.5 rounded-full bg-[#ffbd2e]" />
          <span className="size-2.5 rounded-full bg-[#27c93f]" />
        </span>
        <span className="truncate text-[10px] uppercase tracking-[0.08em] text-[#8b949e]">
          {language ?? 'text'}
        </span>
        <button
          type="button"
          onClick={copyCode}
          className="ml-auto inline-flex items-center gap-1 rounded-md px-2 py-1 text-[11px] text-[#8b949e] transition hover:bg-white/[0.06] hover:text-[#c9d1d9] focus:outline-none"
          aria-label={copied ? 'Code copied' : 'Copy code'}
        >
          {copied ? <CheckCircle2 className="size-3.5 text-success" /> : <Copy className="size-3.5" />}
          <span>{copied ? 'Copied' : 'Copy'}</span>
        </button>
      </div>
      <div className="group/code relative">
        <pre className="!m-0 !rounded-none !border-0 !bg-[#0f1419] !px-4 !py-3.5">{children}</pre>
      </div>
    </div>
  );
}

function extractCodeLanguage(node: ReactNode): string | null {
  if (Array.isArray(node)) {
    for (const child of node) {
      const value = extractCodeLanguage(child);
      if (value) {
        return value;
      }
    }
    return null;
  }
  if (!isValidElement<{ className?: string }>(node)) {
    return null;
  }
  const className = node.props.className ?? '';
  const match = className.match(/language-([a-z0-9_+-]+)/i);
  return match?.[1] ?? null;
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
  if (isUser) {
    return (
      <article className="flex justify-end py-4">
        <div className="max-w-[80%] rounded-[22px] rounded-br-md bg-surface-muted px-[18px] py-3 text-[15px] font-normal leading-[1.55] tracking-[0.06em] text-text [-webkit-text-stroke:0.03px_currentColor]">
          <SkillMentionText
            content={content}
            skills={message.activeSkills}
            className="text-[15px] leading-[1.55]"
          />
        </div>
      </article>
    );
  }

  return (
    <article className="group py-1 pb-8">
      {message.status === 'failed' ? (
        <div className="mb-3 inline-flex rounded-full bg-danger/10 px-2.5 py-1 text-xs font-medium text-danger">
          error
        </div>
      ) : null}
      {showReasoning ? (
        <ReasoningPanel
          reasoning={reasoning}
          streaming={reasoningStreaming}
        />
      ) : null}
      {isStreamingEmpty ? (
        <div className="flex h-7 items-center gap-1 text-text-muted" aria-label="Astra is responding">
          <span className="size-1.5 animate-bounce rounded-full bg-text-muted" />
          <span className="size-1.5 animate-bounce rounded-full bg-text-muted [animation-delay:120ms]" />
          <span className="size-1.5 animate-bounce rounded-full bg-text-muted [animation-delay:240ms]" />
        </div>
      ) : (
        <MarkdownContent content={content} />
      )}
      {message.artifacts?.length ? (
        <ArtifactList artifacts={message.artifacts} />
      ) : null}
      {message.status !== 'streaming' ? (
        <div className="mt-3 flex gap-0.5 opacity-0 transition-opacity group-hover:opacity-100">
          <IconButton icon={Copy} label="Copy response" className="size-8" onClick={() => navigator.clipboard?.writeText(content)} />
          <IconButton icon={RefreshCcw} label="Regenerate response" className="size-8" disabled />
          <IconButton icon={ThumbsUp} label="Good response" className="size-8" />
          <IconButton icon={ThumbsDown} label="Bad response" className="size-8" />
        </div>
      ) : null}
    </article>
  );
}

function ArtifactList({ artifacts }: { artifacts: ChatArtifactRef[] }) {
  const visibleArtifacts = artifacts.filter(isChatVisibleArtifact);
  if (visibleArtifacts.length === 0) {
    return null;
  }

  return (
    <div className="mt-5 space-y-4">
      {visibleArtifacts.map((artifact) => (
        <ArtifactCard key={artifact.id} artifact={artifact} />
      ))}
    </div>
  );
}

function isChatVisibleArtifact(artifact: ChatArtifactRef) {
  return artifact.kind !== 'composite_snapshot_index'
    && artifact.source !== 'composite_snapshot_index';
}

function ArtifactCard({ artifact }: { artifact: ChatArtifactRef }) {
  const content = artifact.content && typeof artifact.content === 'object'
    ? artifact.content as Record<string, unknown>
    : null;
  const payload = buildArtifactPayload(artifact, content);
  const title = artifact.title || artifact.filename || artifact.downloadFilename || artifact.kind;
  const subtitle = [
    artifact.contentType,
    formatBytes(artifact.sizeBytes),
  ].filter(Boolean).join(' · ') || artifact.kind;

  const download = () => {
    if (!payload) {
      return;
    }
    const blob = new Blob([payload.bytes], { type: payload.contentType });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = artifact.downloadFilename || artifact.filename || title;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="overflow-hidden rounded-[18px] border border-border bg-surface shadow-[0_0.25rem_1.25rem_rgba(28,25,23,0.06),0_0_0_0.5px_rgba(120,113,108,0.18)]">
      <div className="flex items-center justify-between gap-4 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <div className="truncate text-sm font-semibold text-text">{title}</div>
          <div className="mt-0.5 text-xs text-text-muted">{subtitle}</div>
        </div>
        <button
          type="button"
          onClick={download}
          disabled={!payload}
          className="inline-flex shrink-0 items-center gap-1.5 rounded-control border border-border bg-bg px-2.5 py-1.5 text-xs font-medium text-text-secondary transition hover:bg-surface-muted hover:text-text disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Download className="size-3.5" />
          Download
        </button>
      </div>
      {payload?.previewKind === 'image' ? (
        <div className="bg-white p-4">
          {/* Artifact previews may be data URLs generated at runtime, so next/image cannot optimize them. */}
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img
            src={payload.previewUrl}
            alt={title}
            className="max-h-[560px] w-full object-contain"
          />
        </div>
      ) : payload?.previewKind === 'text' ? (
        <pre className="max-h-[360px] overflow-auto whitespace-pre-wrap bg-[#0d1117] p-4 font-mono text-sm leading-6 text-[#e6edf3]">
          {payload.previewText}
        </pre>
      ) : (
        <div className="px-4 py-5 text-sm text-text-muted">
          {payload
            ? 'Preview is not available for this artifact type. Use Download to open the file.'
            : 'Artifact payload is not available in this message.'}
        </div>
      )}
    </div>
  );
}

type ArtifactPayload = {
  bytes: BlobPart;
  contentType: string;
  previewKind: 'image' | 'text' | 'none';
  previewUrl?: string;
  previewText?: string;
};

function buildArtifactPayload(
  artifact: ChatArtifactRef,
  content: Record<string, unknown> | null,
): ArtifactPayload | null {
  if (!content) {
    return null;
  }
  const contentType = artifact.contentType || stringValue(content.content_type) || 'application/octet-stream';
  const encoding = stringValue(content.encoding);
  const data = stringValue(content.data);

  if (encoding === 'base64' && data) {
    const bytes = bytesFromBase64(data);
    return {
      bytes,
      contentType,
      previewKind: contentType.startsWith('image/') ? 'image' : 'none',
      previewUrl: contentType.startsWith('image/') ? `data:${contentType};base64,${data}` : undefined,
    };
  }

  if (encoding === 'utf-8' && typeof data === 'string') {
    const previewKind = contentType.startsWith('image/svg+xml') ? 'image' : (isTextPreviewType(contentType) ? 'text' : 'none');
    return {
      bytes: data,
      contentType: contentType.startsWith('text/') ? `${contentType};charset=utf-8` : contentType,
      previewKind,
      previewUrl: previewKind === 'image' ? `data:${contentType};charset=utf-8,${encodeURIComponent(data)}` : undefined,
      previewText: previewKind === 'text' ? truncateArtifactPreview(data) : undefined,
    };
  }

  const legacySvg = stringValue(content.svg);
  if (legacySvg) {
    return {
      bytes: legacySvg,
      contentType: 'image/svg+xml;charset=utf-8',
      previewKind: 'image',
      previewUrl: `data:image/svg+xml;charset=utf-8,${encodeURIComponent(legacySvg)}`,
    };
  }

  return null;
}

function stringValue(value: unknown) {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

function bytesFromBase64(value: string) {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

function isTextPreviewType(contentType: string) {
  return contentType.startsWith('text/')
    || contentType === 'application/json'
    || contentType === 'application/x-ndjson'
    || contentType === 'application/yaml'
    || contentType === 'application/toml'
    || contentType === 'application/xml';
}

function truncateArtifactPreview(value: string) {
  return value.length > 6000 ? `${value.slice(0, 6000)}\n...` : value;
}

function formatBytes(value?: number | null) {
  if (!value || value <= 0) {
    return null;
  }
  if (value < 1024) {
    return `${value} B`;
  }
  if (value < 1024 * 1024) {
    return `${(value / 1024).toFixed(1)} KiB`;
  }
  return `${(value / 1024 / 1024).toFixed(1)} MiB`;
}

function ReasoningPanel({ reasoning, streaming }: { reasoning: string; streaming: boolean }) {
  const [open, setOpen] = useState(streaming);
  const userToggledRef = useRef(false);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const summary = streaming ? 'Thinking' : (reasoning.trim() ? firstLine(reasoning) : 'Done');
  const body = reasoning.trim() || (streaming ? 'Preparing response...' : 'Done');
  const blocks = splitReasoningBlocks(body);

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
    <div className="mb-5 font-sans">
      <button
        type="button"
        onClick={() => {
          userToggledRef.current = true;
          setOpen((value) => !value);
        }}
        aria-expanded={open}
        className="inline-flex max-w-full items-center gap-2 py-1 text-left text-[15px] leading-6 text-text-muted transition-colors hover:text-text-secondary"
      >
        {streaming ? (
          <Loader className="size-4 shrink-0 animate-spin text-warning" />
        ) : null}
        <span className="truncate">{summary}</span>
        <ChevronDown className={cn('size-4 shrink-0 transition-transform duration-200', open && 'rotate-180')} />
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
              'mt-3 pr-2',
              open && (streaming ? 'max-h-56 overflow-y-auto' : 'max-h-[360px] overflow-y-auto'),
            )}
          >
            <div className="space-y-0">
              {blocks.map((block, index) => (
                <ReasoningStep
                  key={`${index}-${block.slice(0, 32)}`}
                  content={block}
                  isLast={index === blocks.length - 1 && streaming}
                />
              ))}
              {!streaming ? (
                <div className="grid grid-cols-[24px_minmax(0,1fr)] gap-3">
                  <div className="flex justify-center">
                    <CheckCircle2 className="mt-0.5 size-5 text-text-muted" strokeWidth={1.8} />
                  </div>
                  <div className="pb-1 text-[16px] leading-6 text-text-secondary">Done</div>
                </div>
              ) : null}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}

function ReasoningStep({ content, isLast }: { content: string; isLast: boolean }) {
  const [expanded, setExpanded] = useState(false);
  const shouldClamp = content.length > 520 || content.split(/\r?\n/).length > 7;

  return (
    <div className="grid grid-cols-[24px_minmax(0,1fr)] gap-3">
      <div className="flex flex-col items-center">
        <Clock3 className="mt-0.5 size-5 text-text-muted" strokeWidth={1.8} />
        {!isLast ? <div className="mt-2 w-px flex-1 bg-border" /> : null}
      </div>
      <div className="min-w-0 pb-5">
        <div
          className={cn(
            'relative min-w-0 overflow-hidden',
            shouldClamp && !expanded && 'max-h-[8.25rem]',
          )}
        >
          <ReasoningMarkdown content={content} />
          {shouldClamp && !expanded ? (
            <div className="pointer-events-none absolute inset-x-0 bottom-0 h-12 bg-gradient-to-b from-transparent to-bg" />
          ) : null}
        </div>
        {shouldClamp ? (
          <button
            type="button"
            onClick={() => setExpanded((value) => !value)}
            className="mt-1 text-[14px] leading-5 text-text-muted transition-colors hover:text-text-secondary"
          >
            {expanded ? 'Show less' : 'Show more'}
          </button>
        ) : null}
      </div>
    </div>
  );
}

function ReasoningMarkdown({ content }: { content: string }) {
  return (
    <div className="astra-reasoning-markdown">
      <ReactMarkdown
        remarkPlugins={markdownRemarkPlugins}
        rehypePlugins={markdownRehypePlugins}
        components={reasoningMarkdownComponents}
      >
        {content}
      </ReactMarkdown>
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

const reasoningMarkdownComponents: Components = {
  p: ({ children }) => <p>{children}</p>,
  ol: ({ children }) => <ol>{children}</ol>,
  ul: ({ children }) => <ul>{children}</ul>,
  li: ({ children }) => <li>{children}</li>,
  code: ({ children }) => <code>{children}</code>,
  pre: ({ children }) => <pre>{children}</pre>,
};

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

function splitReasoningBlocks(text: string) {
  const normalized = text.trim().replace(/\n{3,}/g, '\n\n');
  if (!normalized) {
    return [];
  }
  const paragraphBlocks = normalized
    .split(/\n\s*\n/)
    .map((block) => block.trim())
    .filter(Boolean);
  if (paragraphBlocks.length > 1) {
    return paragraphBlocks;
  }

  const sentenceParts = normalized.match(/[^.!?。！？]+[.!?。！？]?/g)
    ?.map((part) => part.trim())
    .filter(Boolean) ?? [normalized];
  const blocks: string[] = [];
  let current = '';
  for (const part of sentenceParts) {
    if (current && `${current} ${part}`.length > 560) {
      blocks.push(current);
      current = part;
    } else {
      current = current ? `${current} ${part}` : part;
    }
  }
  if (current) {
    blocks.push(current);
  }
  return blocks;
}
