/** Shared framing policy for the Web proxy and browser consumer. */
export function parseChatSseFrame(frame: string, strict = false): Record<string, unknown> | null {
  const data = frame.split(/\r?\n/).filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trim()).join("\n");
  if (!data || data === "[DONE]") return null;
  try {
    const value: unknown = JSON.parse(data);
    if (value && typeof value === "object" && !Array.isArray(value) &&
      typeof (value as Record<string, unknown>).type === "string") return value as Record<string, unknown>;
  } catch { /* handled below; never expose raw event payloads in errors */ }
  if (strict) throw new Error("Incomplete or malformed replay event.");
  return null;
}

/** Indexed errors belong to durable run history, not this replay connection. */
export function isReplayConnectionError(event: Record<string, unknown>): boolean {
  return event.type === "error" &&
    !(typeof event.index === "number" && Number.isSafeInteger(event.index) && event.index >= 0);
}

export function isReplayObservationEvent(event: Record<string, unknown>): boolean {
  return event.type === "explain_analyze" || event.type === "stream_gap" || isReplayConnectionError(event);
}
