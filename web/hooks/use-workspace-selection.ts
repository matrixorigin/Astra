"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { getEdgeStatus, updateChatWorkspaceSelection } from "@/lib/api/chats";
import type { ChatDetail, WorkspaceSelection } from "@/lib/api/types";
import { useToast } from "@/components/ui/toast";

const WORKSPACE_SELECTION_STORAGE_KEY = "astra.web.workspaceSelection";

export type WorkspaceSelectionState = {
  selection: WorkspaceSelection | null;
  explicit: boolean;
};

function workspaceSelectionStorageKey(chatId: string) {
  return `${WORKSPACE_SELECTION_STORAGE_KEY}.${chatId}`;
}

function defaultWorkspaceSelection(): WorkspaceSelection | null {
  return null;
}

function defaultWorkspaceSelectionState(): WorkspaceSelectionState {
  return { selection: defaultWorkspaceSelection(), explicit: false };
}

export function readStoredWorkspaceSelectionState(
  chatId: string,
): WorkspaceSelectionState {
  if (typeof window === "undefined") {
    return defaultWorkspaceSelectionState();
  }
  const raw = window.localStorage.getItem(workspaceSelectionStorageKey(chatId));
  if (!raw) {
    return defaultWorkspaceSelectionState();
  }
  try {
    const value = JSON.parse(raw) as WorkspaceSelection;
    if (value.kind === "server_sandbox") {
      return { selection: value, explicit: true };
    }
    if (
      value.kind === "edge_workspace" &&
      value.edgeAgentId?.trim() &&
      value.cwd?.trim()
    ) {
      return { selection: value, explicit: true };
    }
  } catch {
    // Fall through to the implicit server sandbox display default.
  }
  return defaultWorkspaceSelectionState();
}

export function workspaceSelectionStateFromDetail(
  detail: ChatDetail,
): WorkspaceSelectionState {
  if (
    detail.workspaceSelection &&
    detail.workspaceSelectionExplicit !== false
  ) {
    return { selection: detail.workspaceSelection, explicit: true };
  }
  return readStoredWorkspaceSelectionState(detail.chat.id);
}

function storeWorkspaceSelectionState(
  chatId: string,
  state: WorkspaceSelectionState,
) {
  if (!state.explicit || !state.selection) {
    window.localStorage.removeItem(workspaceSelectionStorageKey(chatId));
    return;
  }
  window.localStorage.setItem(
    workspaceSelectionStorageKey(chatId),
    JSON.stringify(state.selection),
  );
}

export interface UseWorkspaceSelectionParams {
  detail: ChatDetail;
  setDetail: (detail: ChatDetail) => void;
}

export function useWorkspaceSelection(params: UseWorkspaceSelectionParams) {
  const { detail, setDetail } = params;
  const { addToast } = useToast();

  const [workspaceSelectionState, setWorkspaceSelectionState] =
    useState<WorkspaceSelectionState>(() =>
      workspaceSelectionStateFromDetail(detail),
    );
  const [edgeWorkspaces, setEdgeWorkspaces] = useState<
    Awaited<ReturnType<typeof getEdgeStatus>>["edges"]
  >([]);
  const [edgeWorkspacesLoading, setEdgeWorkspacesLoading] = useState(false);
  const [edgeWorkspacesError, setEdgeWorkspacesError] = useState<string | null>(
    null,
  );

  const previousWorkspaceRef = useRef(workspaceSelectionState);
  const workspaceSelectionRequestRef = useRef(0);

  const setWorkspaceSelection = useCallback(
    (selection: WorkspaceSelection) => {
      const previous = previousWorkspaceRef.current;
      const next = { selection, explicit: true };
      const requestId = workspaceSelectionRequestRef.current + 1;
      workspaceSelectionRequestRef.current = requestId;
      previousWorkspaceRef.current = next;
      setWorkspaceSelectionState(next);
      storeWorkspaceSelectionState(detail.chat.id, next);
      void updateChatWorkspaceSelection(detail.chat.id, selection)
        .then((updated) => {
          if (workspaceSelectionRequestRef.current !== requestId) {
            return;
          }
          const updatedState = workspaceSelectionStateFromDetail(updated);
          setDetail(updated);
          setWorkspaceSelectionState(updatedState);
          storeWorkspaceSelectionState(updated.chat.id, updatedState);
          previousWorkspaceRef.current = updatedState;
        })
        .catch((error) => {
          if (workspaceSelectionRequestRef.current !== requestId) {
            return;
          }
          setWorkspaceSelectionState(previous);
          storeWorkspaceSelectionState(detail.chat.id, previous);
          previousWorkspaceRef.current = previous;
          addToast(
            `Environment was not updated. ${
              error instanceof Error
                ? error.message
                : "Failed to save the selected environment."
            }`,
            "warning",
          );
        });
    },
    [addToast, detail.chat.id, setDetail],
  );

  const refreshEdgeWorkspaces = useCallback(async () => {
    setEdgeWorkspacesLoading(true);
    setEdgeWorkspacesError(null);
    try {
      const status = await getEdgeStatus();
      setEdgeWorkspaces(status.edges);
    } catch (error) {
      setEdgeWorkspacesError(
        error instanceof Error
          ? error.message
          : "Failed to load environments.",
      );
    } finally {
      setEdgeWorkspacesLoading(false);
    }
  }, []);

  // Sync detail → localStorage
  useEffect(() => {
    const nextState =
      detail.workspaceSelection && detail.workspaceSelectionExplicit !== false
        ? { selection: detail.workspaceSelection, explicit: true }
        : readStoredWorkspaceSelectionState(detail.chat.id);
    setWorkspaceSelectionState(nextState);
    storeWorkspaceSelectionState(detail.chat.id, nextState);
  }, [
    detail.chat.id,
    detail.workspaceSelection,
    detail.workspaceSelectionExplicit,
  ]);

  // Load edge workspaces on mount
  useEffect(() => {
    void refreshEdgeWorkspaces();
  }, [refreshEdgeWorkspaces]);

  return {
    workspaceSelection: workspaceSelectionState.selection,
    workspaceSelectionExplicit: workspaceSelectionState.explicit,
    workspaceSelectorDisabled: false, // caller computes this from ui state
    edgeWorkspaces,
    edgeWorkspacesLoading,
    edgeWorkspacesError,
    setWorkspaceSelection,
    refreshEdgeWorkspaces,
  };
}
