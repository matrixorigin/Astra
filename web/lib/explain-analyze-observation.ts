import { explainAnalyzeFactFingerprint, type ExplainAnalyzeEventV1 } from "@astra/sdk";
import type { ChatMessage } from "@/lib/api/types";

export const EXPLAIN_FACT_LIMIT = 20_000;

/** A new gap invalidates any repair already in flight. */
export function markExplainGap(message: ChatMessage, unrecoverable = false): ChatMessage {
  return { ...message, explainAnalyzeDegraded: true,
    explainAnalyzeUnrecoverable: message.explainAnalyzeUnrecoverable || unrecoverable,
    explainAnalyzeRepairToken: undefined };
}

export function appendExplainFact(message: ChatMessage, event: ExplainAnalyzeEventV1): ChatMessage {
  const events = message.explainAnalyzeEvents ?? [];
  const fingerprint = explainAnalyzeFactFingerprint(event);
  if (events.some((previous) => previous.event_id === event.event_id &&
    explainAnalyzeFactFingerprint(previous) === fingerprint)) return message;
  if (events.length >= EXPLAIN_FACT_LIMIT) return markExplainGap(message, true);
  return { ...message, explainAnalyzeEvents: [...events, event] };
}

/** Only a complete replay from the run origin may clear a delivery warning. */
export function beginExplainRepair(message: ChatMessage, token: string): ChatMessage {
  return { ...message, explainAnalyzeRepairToken: token };
}

export function finishExplainRepair(message: ChatMessage, token: string): ChatMessage {
  if (message.explainAnalyzeRepairToken !== token) return message;
  return { ...message, explainAnalyzeRepairToken: undefined,
    explainAnalyzeDegraded: message.explainAnalyzeUnrecoverable === true };
}
