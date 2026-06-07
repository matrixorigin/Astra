import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ReactNode } from 'react';
import { ChatView } from '@/components/app/chat-view';
import { WebApiError } from '@/lib/api/errors';
import type { ChatDetail, ComposerOptions } from '@/lib/api/types';
import { getChat, queueChatRunInput, resumeChatRun, stopChatRun, streamChatMessage, streamExistingChatRun, updateChatModel } from '@/lib/api/chats';

const pushMock = jest.fn();
const replaceMock = jest.fn();
const refreshMock = jest.fn();

let composerPayload: {
  text: string;
  options: ComposerOptions;
} = {
  text: 'queue this follow-up',
  options: {
    webSearch: false,
    thinking: true,
    model: 'sonnet-4.6-adaptive',
    activeSkills: [],
  },
};

jest.mock('next/navigation', () => ({
  useRouter: () => ({
    push: pushMock,
    replace: replaceMock,
    refresh: refreshMock,
  }),
}));

jest.mock('next/link', () => ({
  __esModule: true,
  default: ({ children, href }: { children: ReactNode; href: string }) => <a href={href}>{children}</a>,
}));

jest.mock('lucide-react', () => ({
  MoreVertical: () => null,
}));

jest.mock('@/components/app/chat-actions-menu', () => ({
  ChatActionsMenu: () => null,
}));

jest.mock('@/components/app/chat-dot-navigator', () => ({
  ChatDotNavigator: () => null,
}));

jest.mock('@/components/app/move-chat-modal', () => ({
  MoveChatModal: () => null,
}));

jest.mock('@/components/app/message-bubble', () => ({
  MessageBubble: ({ message }: { message: { content: string } }) => <div>{message.content}</div>,
}));

jest.mock('@/components/ui/icon-button', () => ({
  IconButton: () => null,
}));

jest.mock('@/hooks/use-chat-lifecycle-actions', () => ({
  useChatLifecycleActions: () => ({
    busyChatId: null,
    unarchive: jest.fn(),
  }),
}));

jest.mock('@/lib/chat-lifecycle-events', () => ({
  subscribeChatLifecycleChange: () => () => {},
}));

jest.mock('@/components/app/composer', () => ({
  Composer: ({
    disabled,
    onSubmit,
  }: {
    disabled?: boolean;
    onSubmit: (payload: { text: string; attachments: []; options: ComposerOptions }) => Promise<void>;
  }) => (
    <button
      type="button"
      disabled={disabled}
      onClick={() => void onSubmit({
        text: composerPayload.text,
        attachments: [],
        options: composerPayload.options,
      })}
    >
      Submit composer
    </button>
  ),
}));

jest.mock('@/lib/api/chats', () => ({
  getChat: jest.fn(),
  queueChatRunInput: jest.fn(),
  resumeChatRun: jest.fn(),
  stopChatRun: jest.fn(),
  streamChatMessage: jest.fn(),
  streamExistingChatRun: jest.fn(),
  updateChatModel: jest.fn(),
}));

const mockGetChat = getChat as jest.MockedFunction<typeof getChat>;
const mockQueueChatRunInput = queueChatRunInput as jest.MockedFunction<typeof queueChatRunInput>;
const mockResumeChatRun = resumeChatRun as jest.MockedFunction<typeof resumeChatRun>;
const mockStopChatRun = stopChatRun as jest.MockedFunction<typeof stopChatRun>;
const mockStreamChatMessage = streamChatMessage as jest.MockedFunction<typeof streamChatMessage>;
const mockStreamExistingChatRun = streamExistingChatRun as jest.MockedFunction<typeof streamExistingChatRun>;
const mockUpdateChatModel = updateChatModel as jest.MockedFunction<typeof updateChatModel>;

const defaultActiveRun: NonNullable<ChatDetail['activeRun']> = {
  runId: 'run-123',
  status: 'running',
  waitingFor: null,
};

function makeDetail(activeRun: ChatDetail['activeRun'] | null = defaultActiveRun): ChatDetail {
  return {
    chat: {
      id: 'chat-123',
      title: 'Test chat',
      projectId: null,
      createdAt: '2026-06-07T00:00:00.000Z',
      updatedAt: '2026-06-07T00:00:00.000Z',
      archivedAt: null,
      model: 'sonnet-4.6-adaptive',
    },
    messages: [],
    activeRun: activeRun ?? undefined,
  };
}

describe('ChatView deferred-input unhappy paths', () => {
  beforeEach(() => {
    composerPayload = {
      text: 'queue this follow-up',
      options: {
        webSearch: false,
        thinking: true,
        model: 'sonnet-4.6-adaptive',
        activeSkills: [],
      },
    };
    pushMock.mockReset();
    replaceMock.mockReset();
    refreshMock.mockReset();
    mockGetChat.mockReset();
    mockQueueChatRunInput.mockReset();
    mockResumeChatRun.mockReset();
    mockStopChatRun.mockReset();
    mockStreamChatMessage.mockReset();
    mockStreamExistingChatRun.mockReset();
    mockUpdateChatModel.mockReset();
    window.alert = jest.fn();
    HTMLElement.prototype.scrollTo = jest.fn();
  });

  it('does not start a fresh stream when queueing fails for a non-conflict error', async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockRejectedValue(new WebApiError(500, 'runtime temporarily unavailable'));

    render(<ChatView initial={makeDetail()} />);

    await user.click(screen.getByRole('button', { name: 'Submit composer' }));

    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith('chat-123', {
        content: 'queue this follow-up',
        options: composerPayload.options,
      });
    });
    expect(mockGetChat).not.toHaveBeenCalled();
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
    expect(window.alert).toHaveBeenCalledWith('runtime temporarily unavailable');
  });

  it('falls back to a fresh stream only after an explicit stale-run conflict', async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockRejectedValue(new WebApiError(409, 'no active run is available for deferred input'));
    mockGetChat.mockResolvedValue(makeDetail(null));
    mockStreamChatMessage.mockResolvedValue('streamed fallback answer');

    render(<ChatView initial={makeDetail()} />);

    await user.click(screen.getByRole('button', { name: 'Submit composer' }));

    await waitFor(() => {
      expect(mockGetChat).toHaveBeenCalledWith('chat-123');
    });
    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        'chat-123',
        {
          content: 'queue this follow-up',
          options: composerPayload.options,
          pendingMessageId: undefined,
        },
        expect.any(Object),
      );
    });
    expect(window.alert).not.toHaveBeenCalled();
  });

  it('shows an explicit stop action instead of pretending queued input interrupts immediately', async () => {
    const user = userEvent.setup();
    mockStopChatRun.mockResolvedValue({
      activeRun: {
        runId: 'run-123',
        status: 'cancelling',
        waitingFor: null,
      },
    });

    render(<ChatView initial={makeDetail()} />);

    expect(screen.getByText('New messages are queued after the next tool call. Use Stop to interrupt now.')).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Stop' }));

    await waitFor(() => {
      expect(mockStopChatRun).toHaveBeenCalledWith('chat-123');
    });
    expect(screen.getByText('Stopping the current run. New input stays disabled until cancellation completes.')).toBeInTheDocument();
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it('continues queueing follow-up input while the active run is input-queued', async () => {
    const user = userEvent.setup();
    mockQueueChatRunInput.mockResolvedValue({
      userMessage: {
        id: 'queued-user-1',
        role: 'user',
        content: 'queue this follow-up',
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'complete',
      },
      activeRun: {
        runId: 'run-123',
        status: 'input-queued',
        waitingFor: 'user_input',
      },
    });

    render(<ChatView initial={makeDetail({
      runId: 'run-123',
      status: 'input-queued',
      waitingFor: 'user_input',
    })} />);

    expect(screen.getByText(/Input queued for next tool call/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Submit composer' }));

    await waitFor(() => {
      expect(mockQueueChatRunInput).toHaveBeenCalledWith('chat-123', {
        content: 'queue this follow-up',
        options: composerPayload.options,
      });
    });
    expect(mockStreamChatMessage).not.toHaveBeenCalled();
  });

  it('does not queue input for terminal active-run statuses', async () => {
    const user = userEvent.setup();
    mockStreamChatMessage.mockResolvedValue('new answer');

    render(<ChatView initial={makeDetail({
      runId: 'run-123',
      status: 'completed',
      waitingFor: null,
    })} />);

    await user.click(screen.getByRole('button', { name: 'Submit composer' }));

    await waitFor(() => {
      expect(mockStreamChatMessage).toHaveBeenCalledWith(
        'chat-123',
        {
          content: 'queue this follow-up',
          options: composerPayload.options,
          pendingMessageId: undefined,
        },
        expect.objectContaining({
          signal: expect.any(AbortSignal),
        }),
      );
    });
    expect(mockQueueChatRunInput).not.toHaveBeenCalled();
  });

  it('lets the web user resume a paused run instead of trapping the composer', async () => {
    const user = userEvent.setup();
    mockResumeChatRun.mockResolvedValue({
      activeRun: {
        runId: 'run-123',
        status: 'running',
        waitingFor: null,
      },
    });
    mockStreamExistingChatRun.mockResolvedValue('resumed assistant text');

    render(<ChatView initial={{
      ...makeDetail({
        runId: 'run-123',
        status: 'paused',
        waitingFor: null,
      }),
      messages: [{
        id: 'assistant-1',
        role: 'assistant',
        content: 'Partial reply',
        createdAt: '2026-06-07T00:00:00.000Z',
        status: 'streaming',
      }],
    }} />);

    expect(screen.getByText('This run is paused. Resume to continue or Stop to cancel it.')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Resume' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Stop' })).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: 'Resume' }));

    await waitFor(() => {
      expect(mockResumeChatRun).toHaveBeenCalledWith('chat-123');
    });
    await waitFor(() => {
      expect(mockStreamExistingChatRun).toHaveBeenCalledWith(
        'chat-123',
        'run-123',
        expect.objectContaining({
          onRunUpdated: expect.any(Function),
          onDone: expect.any(Function),
          onPaused: expect.any(Function),
        }),
      );
    });
  });
});
