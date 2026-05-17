import type { RefObject } from 'react';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ChatDotNavigator } from '@/components/app/chat-dot-navigator';
import type { ChatMessage } from '@/lib/api/types';

function message(role: ChatMessage['role'], content: string): Pick<ChatMessage, 'role' | 'content'> {
  return { role, content };
}

function userMessage(index: number): Pick<ChatMessage, 'role' | 'content'> {
  return message('user', `User input ${index}`);
}

function renderNavigator(
  messages: Pick<ChatMessage, 'role' | 'content'>[],
  container = document.createElement('div'),
) {
  const scrollContainerRef = { current: container } as RefObject<HTMLDivElement | null>;

  render(<ChatDotNavigator messages={messages} scrollContainerRef={scrollContainerRef} />);
}

function makeScrollableContainer() {
  const container = document.createElement('div');
  const scrollTo = jest.fn();

  Object.defineProperties(container, {
    clientHeight: { configurable: true, value: 400 },
    scrollHeight: { configurable: true, value: 1200 },
    scrollTop: { configurable: true, writable: true, value: 100 },
    scrollTo: { configurable: true, value: scrollTo },
  });
  container.getBoundingClientRect = jest.fn(() => ({ top: 20 }) as DOMRect);

  return { container, scrollTo };
}

function appendMessageAnchor(container: HTMLElement, index: number, top: number) {
  const element = document.createElement('div');
  element.dataset.chatMessageIndex = String(index);
  element.getBoundingClientRect = jest.fn(() => ({ top }) as DOMRect);
  container.appendChild(element);
  return element;
}

describe('ChatDotNavigator', () => {
  it('stays hidden until the first message exists', () => {
    renderNavigator([]);
    expect(screen.queryByRole('navigation', { name: 'Message navigation' })).not.toBeInTheDocument();
  });

  it('appears with the first message', () => {
    renderNavigator([message('user', 'Build a dashboard')]);
    expect(screen.getByRole('navigation', { name: 'Message navigation' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Jump to Msg 1: Build a dashboard' })).toBeInTheDocument();
    expect(screen.getByText('Msg 1: Build a dashboard')).toBeInTheDocument();
  });

  it('anchors every message in short conversations', () => {
    renderNavigator([
      userMessage(1),
      message('assistant', 'Assistant response 1'),
      userMessage(2),
      message('assistant', 'Assistant response 2'),
      userMessage(3),
    ]);

    expect(screen.getAllByRole('button')).toHaveLength(5);
    for (let index = 1; index <= 5; index += 1) {
      expect(screen.getByRole('button', { name: new RegExp(`^Jump to Msg ${index}:`) })).toBeInTheDocument();
    }
  });

  it('labels assistant anchors with the preceding user input', () => {
    renderNavigator([
      message('user', 'Explain retries'),
      message('assistant', 'Use backoff'),
    ]);

    expect(screen.getByRole('button', { name: 'Jump to Msg 2: Explain retries' })).toBeInTheDocument();
  });

  it('compacts long user inputs in labels', () => {
    renderNavigator([
      message(
        'user',
        'Summarize this long request with multiple\n\nspaces and enough extra words to require a compact preview in the navigator tooltip.',
      ),
    ]);

    expect(screen.getByText('Msg 1: Summarize this long request with multiple spaces and enough extra words to...')).toBeInTheDocument();
  });

  it('scrolls the chat container to an anchored message', async () => {
    const user = userEvent.setup();
    const { container, scrollTo } = makeScrollableContainer();
    appendMessageAnchor(container, 0, 220);

    renderNavigator([
      message('user', 'First task'),
      message('assistant', 'First response'),
    ], container);

    await user.click(screen.getByRole('button', { name: 'Jump to Msg 1: First task' }));

    expect(scrollTo).toHaveBeenCalledWith({ top: 300, behavior: 'smooth' });
  });

  it('scrolls to the bottom for the last message anchor', async () => {
    const user = userEvent.setup();
    const { container, scrollTo } = makeScrollableContainer();
    appendMessageAnchor(container, 3, 700);

    renderNavigator([
      message('user', 'First task'),
      message('assistant', 'First response'),
      message('user', 'Second task'),
      message('assistant', 'Second response'),
    ], container);

    await user.click(screen.getByRole('button', { name: 'Jump to Msg 4: Second task' }));

    expect(scrollTo).toHaveBeenCalledWith({ top: 800, behavior: 'smooth' });
  });
});
