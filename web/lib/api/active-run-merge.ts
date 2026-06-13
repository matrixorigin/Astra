import type { ChatDetail } from "@/lib/api/types";

export type ChatActiveRunSource = "backend_poll" | "stream" | "local_mutation";

export type ChatActiveRunRecord = NonNullable<ChatDetail["activeRun"]> & {
  source: ChatActiveRunSource;
  observedAt: string;
};

function nowIso(): string {
  return new Date().toISOString();
}

function normalizeNextEventIndex(value: unknown): number | null {
  if (typeof value !== "number" || !Number.isFinite(value) || value < 0) {
    return null;
  }
  return Math.trunc(value);
}

function maxNextEventIndex(
  left: number | null | undefined,
  right: number | null | undefined,
): number | null {
  const normalizedLeft = normalizeNextEventIndex(left);
  const normalizedRight = normalizeNextEventIndex(right);
  if (normalizedLeft === null) {
    return normalizedRight;
  }
  if (normalizedRight === null) {
    return normalizedLeft;
  }
  return Math.max(normalizedLeft, normalizedRight);
}

export function makeActiveRunRecord(
  activeRun: NonNullable<ChatDetail["activeRun"]>,
  source: ChatActiveRunSource,
  observedAt = nowIso(),
): ChatActiveRunRecord {
  const assistantMessageId =
    typeof activeRun.assistantMessageId === "string" &&
    activeRun.assistantMessageId.trim()
      ? activeRun.assistantMessageId
      : null;
  const nextEventIndex = normalizeNextEventIndex(activeRun.nextEventIndex);
  return {
    runId: activeRun.runId,
    status: activeRun.status,
    waitingFor: activeRun.waitingFor ?? null,
    ...(assistantMessageId ? { assistantMessageId } : {}),
    ...(nextEventIndex !== null ? { nextEventIndex } : {}),
    source,
    observedAt,
  };
}

export function mergeRunStreamBinding(
  activeRun: ChatActiveRunRecord,
  existing?: NonNullable<ChatDetail["activeRun"]>,
): ChatActiveRunRecord {
  if (!existing || existing.runId !== activeRun.runId) {
    return activeRun;
  }
  const assistantMessageId =
    activeRun.assistantMessageId ?? existing.assistantMessageId;
  const nextEventIndex = maxNextEventIndex(
    activeRun.nextEventIndex,
    existing.nextEventIndex,
  );
  return {
    ...activeRun,
    ...(assistantMessageId ? { assistantMessageId } : {}),
    ...(nextEventIndex !== null ? { nextEventIndex } : {}),
  };
}

export function mergeStreamRunUpdate(
  run: {
    runId: string;
    status: string;
    waitingFor?: string | null;
    assistantMessageId?: string | null;
    nextEventIndex?: number | null;
  },
  existingActiveRun: ChatDetail["activeRun"] | undefined,
): NonNullable<ChatDetail["activeRun"]> {
  const base = makeActiveRunRecord(
    {
      runId: run.runId,
      status: run.status,
      assistantMessageId: run.assistantMessageId,
      nextEventIndex: run.nextEventIndex,
      waitingFor: run.waitingFor,
    },
    "stream",
  );

  return mergeRunStreamBinding(base, existingActiveRun);
}
