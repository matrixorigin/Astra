export type ThinkingSplit = {
  visibleText: string;
  reasoning: string;
  hasThinking: boolean;
  reasoningOpen: boolean;
};

const THINKING_TAGS = [
  ["<thinking>", "</thinking>"],
  ["<think>", "</think>"],
] as const;

export function mergeTextDelta(current: string, delta: string) {
  if (!delta || current === delta || current.endsWith(delta)) {
    return current;
  }
  if (delta.slice(0, current.length) === current) {
    return delta;
  }
  return `${current}${delta}`;
}

export function splitThinkingTags(text: string): ThinkingSplit {
  const lower = text.toLowerCase();
  let cursor = 0;
  let visibleText = "";
  let reasoning = "";
  let hasThinking = false;
  let reasoningOpen = false;

  for (;;) {
    let match: { openIndex: number; openTag: string; closeTag: string } | null =
      null;
    for (const [openTag, closeTag] of THINKING_TAGS) {
      const openIndex = lower.indexOf(openTag, cursor);
      if (openIndex !== -1 && (!match || openIndex < match.openIndex)) {
        match = { openIndex, openTag, closeTag };
      }
    }

    if (!match) {
      let orphanClose: { closeIndex: number; closeTag: string } | null = null;
      for (const [, closeTag] of THINKING_TAGS) {
        const closeIndex = lower.indexOf(closeTag, cursor);
        if (
          closeIndex !== -1 &&
          (!orphanClose || closeIndex < orphanClose.closeIndex)
        ) {
          orphanClose = { closeIndex, closeTag };
        }
      }

      if (orphanClose) {
        hasThinking = true;
        reasoning += text.slice(cursor, orphanClose.closeIndex);
        cursor = orphanClose.closeIndex + orphanClose.closeTag.length;
        continue;
      }

      visibleText += text.slice(cursor);
      break;
    }

    hasThinking = true;
    visibleText += text.slice(cursor, match.openIndex);
    const reasoningStart = match.openIndex + match.openTag.length;
    const closeIndex = lower.indexOf(match.closeTag, reasoningStart);

    if (closeIndex === -1) {
      reasoning += text.slice(reasoningStart);
      reasoningOpen = true;
      break;
    }

    reasoning += text.slice(reasoningStart, closeIndex);
    cursor = closeIndex + match.closeTag.length;
  }

  return {
    visibleText: visibleText.replace(/\n{3,}/g, "\n\n").trim(),
    reasoning: reasoning.replace(/\n{3,}/g, "\n\n").trim(),
    hasThinking,
    reasoningOpen,
  };
}
