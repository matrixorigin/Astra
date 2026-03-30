'use client';

import { useState, useCallback, memo } from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import rehypeHighlight from 'rehype-highlight';

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);

  const handleCopy = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    } catch {
      // Clipboard API not available
    }
  }, [text]);

  return (
    <button
      type="button"
      onClick={handleCopy}
      className="absolute right-2 top-2 rounded-md bg-slate-700/80 px-2 py-1 text-[10px] text-slate-300 opacity-0 transition-opacity hover:bg-slate-600 group-hover/code:opacity-100"
    >
      {copied ? '✓ Copied' : 'Copy'}
    </button>
  );
}

export const MarkdownRenderer = memo(function MarkdownRenderer({
  content,
  className = '',
}: {
  content: string;
  className?: string;
}) {
  return (
    <div className={`prose prose-invert prose-sm max-w-none ${className}`}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeHighlight]}
        components={{
          // Code blocks with copy button and syntax highlighting
          pre({ children, ...props }) {
            // Extract text content for copy
            const codeText = extractText(children);
            return (
              <div className="group/code relative">
                <pre
                  className="overflow-x-auto rounded-lg border border-slate-700/50 bg-slate-900/80 p-4 text-[13px] leading-relaxed"
                  {...props}
                >
                  {children}
                </pre>
                <CopyButton text={codeText} />
              </div>
            );
          },
          // Inline code
          code({ className: codeClassName, children, ...props }) {
            // If it has a language class, it's inside a <pre> — rendered by rehype-highlight
            const isBlock = codeClassName?.startsWith('hljs') || codeClassName?.startsWith('language-');
            if (isBlock) {
              return (
                <code className={codeClassName} {...props}>
                  {children}
                </code>
              );
            }
            return (
              <code
                className="rounded bg-slate-800 px-1.5 py-0.5 text-[13px] text-sky-300"
                {...props}
              >
                {children}
              </code>
            );
          },
          // Links
          a({ children, ...props }) {
            return (
              <a
                className="text-sky-400 underline decoration-sky-400/30 hover:decoration-sky-400"
                target="_blank"
                rel="noopener noreferrer"
                {...props}
              >
                {children}
              </a>
            );
          },
          // Tables
          table({ children, ...props }) {
            return (
              <div className="overflow-x-auto">
                <table className="border-collapse border border-slate-700" {...props}>
                  {children}
                </table>
              </div>
            );
          },
          th({ children, ...props }) {
            return (
              <th className="border border-slate-700 bg-slate-800/50 px-3 py-2 text-left text-xs" {...props}>
                {children}
              </th>
            );
          },
          td({ children, ...props }) {
            return (
              <td className="border border-slate-700 px-3 py-2 text-xs" {...props}>
                {children}
              </td>
            );
          },
          // Blockquotes
          blockquote({ children, ...props }) {
            return (
              <blockquote
                className="border-l-2 border-sky-500/50 pl-4 text-slate-400 italic"
                {...props}
              >
                {children}
              </blockquote>
            );
          },
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
});

/** Extract plain text from React children for clipboard. */
function extractText(node: React.ReactNode): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(extractText).join('');
  if (node && typeof node === 'object' && 'props' in node) {
    const el = node as { props?: { children?: React.ReactNode } };
    return extractText(el.props?.children);
  }
  return '';
}
