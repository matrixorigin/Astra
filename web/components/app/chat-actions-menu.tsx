'use client';

import { Archive, ArchiveRestore, Download, Edit3, FolderInput, Trash2 } from 'lucide-react';
import type { ReactNode } from 'react';
import { useRef, useState } from 'react';
import { Menu, MenuConfirmPanel, MenuItem } from '@/components/ui/menu';
import { useChatLifecycleActions } from '@/hooks/use-chat-lifecycle-actions';
import type { ChatDetail } from '@/lib/api/types';

type ChatActionsMenuProps = {
  chatId: string;
  archived?: boolean;
  active?: boolean;
  afterMutationHref?: string;
  trigger: ReactNode;
  variant?: 'full' | 'compact';
  onMove?: () => void;
  onChatUpdated?: (chat: ChatDetail) => void;
};

export function ChatActionsMenu({
  chatId,
  archived = false,
  active = false,
  afterMutationHref,
  trigger,
  variant = 'full',
  onMove,
  onChatUpdated,
}: ChatActionsMenuProps) {
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [localBusy, setLocalBusy] = useState(false);
  const pendingRef = useRef(false);
  const lifecycle = useChatLifecycleActions({ onChatUpdated });
  const busy = localBusy || lifecycle.busyChatId === chatId;
  const activeRedirect = active ? { redirectHref: afterMutationHref ?? '/chats', replace: true } : undefined;

  async function runArchive(nextArchived: boolean) {
    if (pendingRef.current) {
      return;
    }
    pendingRef.current = true;
    setLocalBusy(true);
    const result = nextArchived
      ? await lifecycle.archive(chatId, activeRedirect)
      : await lifecycle.unarchive(chatId);
    if (!result || (!nextArchived && active)) {
      pendingRef.current = false;
      setLocalBusy(false);
    }
  }

  async function runDelete() {
    if (pendingRef.current) {
      return;
    }
    pendingRef.current = true;
    setLocalBusy(true);
    const deleted = await lifecycle.permanentlyDelete(chatId, activeRedirect);
    if (!deleted) {
      pendingRef.current = false;
      setLocalBusy(false);
    }
  }

  return (
    <Menu
      onOpenChange={(open) => {
        if (!open) {
          setConfirmingDelete(false);
        }
      }}
      trigger={trigger}
    >
      {confirmingDelete ? (
        <MenuConfirmPanel
          destructive
          busy={busy}
          message="Delete this chat permanently? The remote session will also be deleted."
          confirmLabel="Delete"
          onCancel={() => setConfirmingDelete(false)}
          onConfirm={() => {
            void runDelete();
          }}
        />
      ) : (
        <>
          {variant === 'full' ? (
            <>
              <MenuItem icon={Edit3} disabled>Rename</MenuItem>
              <MenuItem icon={FolderInput} disabled={busy || !onMove} onSelect={onMove}>
                Move to another project
              </MenuItem>
            </>
          ) : null}

          {archived ? (
            <MenuItem
              icon={ArchiveRestore}
              disabled={busy}
              onSelect={() => {
                void runArchive(false);
              }}
            >
              Unarchive
            </MenuItem>
          ) : (
            <MenuItem
              icon={Archive}
              disabled={busy}
              onSelect={() => {
                void runArchive(true);
              }}
            >
              Archive
            </MenuItem>
          )}

          {variant === 'full' ? <MenuItem icon={Download} disabled>Export</MenuItem> : null}

          <MenuItem
            icon={Trash2}
            destructive
            disabled={busy}
            onSelect={(event) => {
              event.preventDefault();
              setConfirmingDelete(true);
            }}
          >
            Delete permanently
          </MenuItem>
        </>
      )}
    </Menu>
  );
}
