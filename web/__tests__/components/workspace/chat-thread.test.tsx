import React from 'react';
import { render, screen } from '@testing-library/react';
import type { ChatMessage } from '@/lib/workspace/types';

// Mock heavy sub-components to avoid import issues
jest.mock('@/components/workspace/markdown-renderer', () => ({
  MarkdownRenderer: ({ content }: { content: string }) => (
    <div data-testid="markdown">{content}</div>
  ),
}));
jest.mock('@/components/workspace/thinking-block', () => ({
  ThinkingBlock: ({ thinking }: { thinking: { content: string } }) => (
    <div data-testid="thinking">{thinking.content}</div>
  ),
}));

import { ChatThread } from '@/components/workspace/chat-thread';

describe('ChatThread', () => {
  it('renders empty state when no messages', () => {
    render(<ChatThread messages={[]} />);
    expect(screen.getByText('Send a message to start the conversation.')).toBeInTheDocument();
  });

  it('renders user message content', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'user', content: 'Hello there!', timestamp: Date.now() },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByText('Hello there!')).toBeInTheDocument();
  });

  it('renders assistant message via MarkdownRenderer', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'assistant', content: 'Hi back!', timestamp: Date.now() },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByTestId('markdown')).toHaveTextContent('Hi back!');
  });

  it('renders both user and assistant messages', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'user', content: 'Question', timestamp: Date.now() },
      { id: '2', role: 'assistant', content: 'Answer', timestamp: Date.now() },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByText('Question')).toBeInTheDocument();
    expect(screen.getByTestId('markdown')).toHaveTextContent('Answer');
  });

  it('shows thinking block for assistant messages with thinking', () => {
    const messages: ChatMessage[] = [
      {
        id: '1',
        role: 'assistant',
        content: 'Result',
        timestamp: Date.now(),
        thinking: { content: 'Let me think...', done: true },
      },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByTestId('thinking')).toHaveTextContent('Let me think...');
  });

  it('shows "Thinking…" placeholder for streaming assistant with no content', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'assistant', content: '', timestamp: Date.now(), streaming: true },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByText('Thinking…')).toBeInTheDocument();
  });

  it('does not show "Thinking…" when thinking block exists', () => {
    const messages: ChatMessage[] = [
      {
        id: '1',
        role: 'assistant',
        content: '',
        timestamp: Date.now(),
        streaming: true,
        thinking: { content: 'reasoning', done: false },
      },
    ];
    render(<ChatThread messages={messages} />);
    // The thinking block is present, so the "Thinking…" text should not appear
    // (the code renders '' when thinking exists and content is empty)
    expect(screen.queryByText('Thinking…')).not.toBeInTheDocument();
  });

  it('shows streaming cursor when streaming and content is not empty', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'assistant', content: 'In progress', timestamp: Date.now(), streaming: true },
    ];
    const { container } = render(<ChatThread messages={messages} />);
    // Streaming cursor is a span with animate-pulse and bg-sky-400/60
    const cursor = container.querySelector('.animate-pulse.bg-sky-400\\/60');
    expect(cursor).toBeInTheDocument();
  });

  it('does not show streaming cursor for completed messages', () => {
    const messages: ChatMessage[] = [
      { id: '1', role: 'assistant', content: 'Done', timestamp: Date.now(), streaming: false },
    ];
    const { container } = render(<ChatThread messages={messages} />);
    const cursor = container.querySelector('.bg-sky-400\\/60');
    expect(cursor).not.toBeInTheDocument();
  });

  it('shows tool call summary', () => {
    const messages: ChatMessage[] = [
      {
        id: '1',
        role: 'assistant',
        content: 'Used tools',
        timestamp: Date.now(),
        toolCalls: [
          { callId: 'tc-1', tool: 'file_search', status: 'done', startedAt: Date.now() },
          { callId: 'tc-2', tool: 'code_edit', status: 'running', startedAt: Date.now() },
        ],
      },
    ];
    render(<ChatThread messages={messages} />);
    expect(screen.getByText('file_search')).toBeInTheDocument();
    expect(screen.getByText('code_edit')).toBeInTheDocument();
  });

  it('applies custom className', () => {
    const { container } = render(<ChatThread messages={[]} className="my-custom-class" />);
    expect(container.firstChild).toHaveClass('my-custom-class');
  });
});
