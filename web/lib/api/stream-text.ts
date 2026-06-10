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

const DSML_PREFIX = "[|\\uFF5C]{2}DSML[|\\uFF5C]{2}";
const DSML_TOOL_CALL_OPEN_RE = new RegExp(
  `<\\s*${DSML_PREFIX}tool_calls\\s*>`,
  "i",
);
const DSML_TOOL_CALL_CLOSE_RE = new RegExp(
  `<\\s*\\/\\s*${DSML_PREFIX}tool_calls\\s*>`,
  "i",
);

export function mergeTextDelta(current: string, delta: string) {
  if (!delta || current === delta || current.endsWith(delta)) {
    return current;
  }
  if (delta.slice(0, current.length) === current) {
    return delta;
  }
  return `${current}${delta}`;
}

export function stripDsmlToolCallBlocks(text: string) {
  let cursor = 0;
  let visible = "";

  for (;;) {
    const tail = text.slice(cursor);
    const open = DSML_TOOL_CALL_OPEN_RE.exec(tail);
    if (!open || open.index === undefined) {
      visible += tail;
      break;
    }

    const openStart = cursor + open.index;
    visible += text.slice(cursor, openStart);
    const bodyStart = openStart + open[0].length;
    const close = DSML_TOOL_CALL_CLOSE_RE.exec(text.slice(bodyStart));
    if (!close || close.index === undefined) {
      cursor = text.length;
      break;
    }
    cursor = bodyStart + close.index + close[0].length;
  }

  return visible.replace(/[ \t]+\n/g, "\n").replace(/\n{3,}/g, "\n\n").trim();
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
    visibleText: stripDsmlToolCallBlocks(visibleText),
    reasoning: stripDsmlToolCallBlocks(reasoning),
    hasThinking,
    reasoningOpen,
  };
}
