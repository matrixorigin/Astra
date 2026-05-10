'use client';

import { useCallback, useState } from 'react';
import { useRouter } from 'next/navigation';
import {
  archiveChat as archiveChatRequest,
  clearArchivedChats as clearArchivedChatsRequest,
  deleteChat as deleteChatRequest,
} from '@/lib/api/chats';
import { emitChatLifecycleChange } from '@/lib/chat-lifecycle-events';
import type { ChatDetail } from '@/lib/api/types';

type ActionOptions = {
  redirectHref?: string;
  redirectToChats?: boolean;
  replace?: boolean;
};

type UseChatLifecycleActionsOptions = {
  onChatUpdated?: (chat: ChatDetail) => void;
  onError?: (message: string) => void;
};

function errorMessage(error: unknown, fallback: string) {
  return error instanceof Error ? error.message : fallback;
}

function navigationHref(options: ActionOptions) {
  if (options.redirectHref) {
    return options.redirectHref;
  }
  if (options.redirectToChats) {
    return '/chats';
  }
  return null;
}

export function useChatLifecycleActions({
  onChatUpdated,
  onError,
}: UseChatLifecycleActionsOptions = {}) {
  const router = useRouter();
  const [busyChatId, setBusyChatId] = useState<string | null>(null);
  const [clearingArchived, setClearingArchived] = useState(false);

  const reportError = useCallback((message: string) => {
    if (onError) {
      onError(message);
      return;
    }
    window.alert(message);
  }, [onError]);

  const setArchived = useCallback(async (
    chatId: string,
    archived: boolean,
    options: ActionOptions = {},
  ) => {
    try {
      setBusyChatId(chatId);
      const chat = await archiveChatRequest(chatId, archived);
      onChatUpdated?.(chat);
      emitChatLifecycleChange({
        action: archived ? 'archive' : 'unarchive',
        chatId,
        archived,
        chat,
      });
      const href = navigationHref(options);
      if (href) {
        if (options.replace) {
          router.replace(href);
        } else {
          router.push(href);
        }
      } else {
        router.refresh();
      }
      return chat;
    } catch (error) {
      reportError(errorMessage(error, 'Failed to update archive state.'));
      return null;
    } finally {
      setBusyChatId(null);
    }
  }, [onChatUpdated, reportError, router]);

  const permanentlyDelete = useCallback(async (
    chatId: string,
    options: ActionOptions = {},
  ) => {
    try {
      setBusyChatId(chatId);
      await deleteChatRequest(chatId);
      emitChatLifecycleChange({ action: 'delete', chatId });
      const href = navigationHref(options);
      if (href) {
        if (options.replace) {
          router.replace(href);
        } else {
          router.push(href);
        }
      } else {
        router.refresh();
      }
      return true;
    } catch (error) {
      reportError(errorMessage(error, 'Failed to delete chat.'));
      return false;
    } finally {
      setBusyChatId(null);
    }
  }, [reportError, router]);

  const clearArchived = useCallback(async (options: ActionOptions = {}) => {
    try {
      setClearingArchived(true);
      const result = await clearArchivedChatsRequest();
      emitChatLifecycleChange({ action: 'clearArchived' });
      const href = navigationHref(options);
      if (href) {
        if (options.replace) {
          router.replace(href);
        } else {
          router.push(href);
        }
      } else {
        router.refresh();
      }
      return result.deleted;
    } catch (error) {
      reportError(errorMessage(error, 'Failed to clear archived chats.'));
      return null;
    } finally {
      setClearingArchived(false);
    }
  }, [reportError, router]);

  return {
    busyChatId,
    clearingArchived,
    setArchived,
    archive: (chatId: string, options?: ActionOptions) => setArchived(chatId, true, options),
    unarchive: (chatId: string, options?: ActionOptions) => setArchived(chatId, false, options),
    permanentlyDelete,
    clearArchived,
  };
}
