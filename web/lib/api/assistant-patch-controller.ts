import type { Dispatch, SetStateAction } from "react";
import type { ChatDetail, ChatMessage } from "@/lib/api/types";
import { mergeChatArtifacts } from "@/lib/api/stream-artifacts";

export function createAssistantPatchController(params: {
  setDetail: Dispatch<SetStateAction<ChatDetail>>;
  getAssistantId: () => string;
}) {
  let framePatch: Partial<ChatMessage> | null = null;
  let frameRaf: number | null = null;
  let mounted = true;

  const applyPatch = (assistantId: string, patch: Partial<ChatMessage>) => {
    if (!mounted) return;
    params.setDetail((current) => ({
      ...current,
      messages: current.messages.map((message) => {
        if (message.id !== assistantId) {
          return message;
        }
        return {
          ...message,
          ...patch,
          ...(patch.artifacts
            ? {
                artifacts: mergeChatArtifacts(
                  message.artifacts ?? [],
                  patch.artifacts,
                ),
              }
            : {}),
        };
      }),
    }));
  };

  const flush = () => {
    const patch = framePatch;
    const assistantId = params.getAssistantId();
    framePatch = null;
    frameRaf = null;
    if (patch) {
      applyPatch(assistantId, patch);
    }
  };

  return {
    patchNow(patch: Partial<ChatMessage>) {
      applyPatch(params.getAssistantId(), patch);
    },
    patchBatched(patch: Partial<ChatMessage>) {
      if (!mounted) return;
      framePatch = { ...framePatch, ...patch };
      if (frameRaf === null) {
        frameRaf = requestAnimationFrame(flush);
      }
    },
    flushNow() {
      if (frameRaf !== null) {
        cancelAnimationFrame(frameRaf);
        frameRaf = null;
      }
      flush();
    },
    cancel() {
      mounted = false;
      if (frameRaf !== null) {
        cancelAnimationFrame(frameRaf);
        frameRaf = null;
      }
      framePatch = null;
    },
  };
}
