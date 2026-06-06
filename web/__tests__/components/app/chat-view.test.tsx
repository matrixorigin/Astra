import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ReactNode } from 'react';
import { ChatView } from '@/components/app/chat-view';
import { WebApiError } from '@/lib/api/errors';
import type { ChatDetail, ComposerOptions } from '@/lib/api/types';
import { getChat, queueChatRunInput, streamChatMessage, updateChatModel } from '@/lib/api/chats';

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
  streamChatMessage: jest.fn(),
  updateChatModel: jest.fn(),
}));

const mockGetChat = getChat as jest.MockedFunction<typeof getChat>;
const mockQueueChatRunInput = queueChatRunInput as jest.MockedFunction<typeof queueChatRunInput>;
const mockStreamChatMessage = streamChatMessage as jest.MockedFunction<typeof streamChatMessage>;
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
    mockStreamChatMessage.mockReset();
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
});
