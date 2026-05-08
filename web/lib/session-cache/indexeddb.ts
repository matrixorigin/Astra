import type { StreamEvent } from '@/lib/streaming/types';

export const SESSION_CACHE_DB = 'astra-web-session-cache';
export const SESSION_CACHE_VERSION = 1;
export const WATERMARK_CHANNEL = 'astra-session-watermarks';
export const SSE_CLIENT_DEAD_TIMEOUT_MS = 45_000;

type WatermarkRecord = {
  sessionId: string;
  runEventHighWatermark: number;
  transcriptHighWatermark: number;
  stateRevision: number;
  updatedAt: number;
};

export type ApplyRunEventsResult = {
  applied: number;
  duplicate: number;
  lastOkIdx: number;
  gapDetected: boolean;
  reconnectLastIndex: number;
};

function requestToPromise<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB request failed'));
  });
}

function txDone(tx: IDBTransaction): Promise<void> {
  return new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onabort = () => reject(tx.error ?? new Error('IndexedDB transaction aborted'));
    tx.onerror = () => reject(tx.error ?? new Error('IndexedDB transaction failed'));
  });
}

export function openSessionCache(): Promise<IDBDatabase> {
  if (typeof indexedDB === 'undefined') {
    return Promise.reject(new Error('IndexedDB unavailable'));
  }
  return new Promise((resolve, reject) => {
    const request = indexedDB.open(SESSION_CACHE_DB, SESSION_CACHE_VERSION);
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains('run_events')) {
        db.createObjectStore('run_events', { keyPath: 'cacheKey' });
      }
      if (!db.objectStoreNames.contains('session_watermarks')) {
        db.createObjectStore('session_watermarks', { keyPath: 'sessionId' });
      }
      if (!db.objectStoreNames.contains('transcript_items')) {
        db.createObjectStore('transcript_items', { keyPath: 'cacheKey' });
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error ?? new Error('IndexedDB open failed'));
  });
}

export async function readWatermark(sessionId: string): Promise<WatermarkRecord | null> {
  if (typeof indexedDB === 'undefined') return null;
  const db = await openSessionCache();
  try {
    const tx = db.transaction('session_watermarks', 'readonly');
    const record = await requestToPromise<WatermarkRecord | undefined>(
      tx.objectStore('session_watermarks').get(sessionId),
    );
    return record ?? null;
  } finally {
    db.close();
  }
}

export async function applyRunEventsTransaction(
  sessionId: string,
  runId: string,
  events: StreamEvent[],
  currentLastOkIdx: number,
): Promise<ApplyRunEventsResult> {
  if (typeof indexedDB === 'undefined') {
    const indexed = events
      .map((event) => event.index)
      .filter((value): value is number => typeof value === 'number');
    const lastOkIdx = indexed.reduce((max, value) => Math.max(max, value), currentLastOkIdx);
    return { applied: indexed.length, duplicate: 0, lastOkIdx, gapDetected: false, reconnectLastIndex: lastOkIdx };
  }
  const indexedEvents = events.filter((event) => typeof event.index === 'number');
  let lastOkIdx = currentLastOkIdx;
  let applied = 0;
  let duplicate = 0;

  for (const event of indexedEvents) {
    const eventIdx = event.index as number;
    if (eventIdx > lastOkIdx + 1) {
      return {
        applied,
        duplicate,
        lastOkIdx,
        gapDetected: true,
        reconnectLastIndex: lastOkIdx,
      };
    }
    if (eventIdx > lastOkIdx) {
      lastOkIdx = eventIdx;
    }
  }

  const db = await openSessionCache();
  try {
    const tx = db.transaction(['run_events', 'session_watermarks'], 'readwrite');
    const eventsStore = tx.objectStore('run_events');
    const watermarksStore = tx.objectStore('session_watermarks');

    for (const event of indexedEvents) {
      const eventIdx = event.index as number;
      const cacheKey = `${sessionId}|${runId}|${eventIdx}`;
      const existing = await requestToPromise(eventsStore.get(cacheKey));
      if (existing) {
        duplicate += 1;
        continue;
      }
      await requestToPromise(
        eventsStore.put({
          cacheKey,
          sessionId,
          runId,
          eventIdx,
          event,
          appliedAt: Date.now(),
        }),
      );
      applied += 1;
    }

    const existingWatermark = await requestToPromise<WatermarkRecord | undefined>(
      watermarksStore.get(sessionId),
    );
    await requestToPromise(
      watermarksStore.put({
        sessionId,
        runEventHighWatermark: Math.max(
          existingWatermark?.runEventHighWatermark ?? -1,
          lastOkIdx,
        ),
        transcriptHighWatermark: existingWatermark?.transcriptHighWatermark ?? 0,
        stateRevision: existingWatermark?.stateRevision ?? 0,
        updatedAt: Date.now(),
      } satisfies WatermarkRecord),
    );

    await txDone(tx);
    broadcastWatermark(sessionId, lastOkIdx);
    return { applied, duplicate, lastOkIdx, gapDetected: false, reconnectLastIndex: lastOkIdx };
  } catch (error) {
    throw error;
  } finally {
    db.close();
  }
}

export async function applyTranscriptItemsTransaction(
  sessionId: string,
  items: Array<{ item_seq: number; role: string; content: string }>,
): Promise<void> {
  if (typeof indexedDB === 'undefined') return;
  const db = await openSessionCache();
  try {
    const tx = db.transaction(['transcript_items', 'session_watermarks'], 'readwrite');
    const transcriptStore = tx.objectStore('transcript_items');
    const watermarksStore = tx.objectStore('session_watermarks');
    let high = 0;
    for (const item of items) {
      high = Math.max(high, item.item_seq);
      await requestToPromise(
        transcriptStore.put({
          cacheKey: `${sessionId}|${item.item_seq}`,
          sessionId,
          ...item,
          appliedAt: Date.now(),
        }),
      );
    }
    const existing = await requestToPromise<WatermarkRecord | undefined>(
      watermarksStore.get(sessionId),
    );
    await requestToPromise(
      watermarksStore.put({
        sessionId,
        runEventHighWatermark: existing?.runEventHighWatermark ?? -1,
        transcriptHighWatermark: Math.max(existing?.transcriptHighWatermark ?? 0, high),
        stateRevision: existing?.stateRevision ?? 0,
        updatedAt: Date.now(),
      } satisfies WatermarkRecord),
    );
    await txDone(tx);
  } finally {
    db.close();
  }
}

export function broadcastWatermark(sessionId: string, runEventHighWatermark: number): void {
  if (typeof BroadcastChannel === 'undefined') return;
  const channel = new BroadcastChannel(WATERMARK_CHANNEL);
  channel.postMessage({ sessionId, runEventHighWatermark, sentAt: Date.now() });
  channel.close();
}

export function subscribeWatermarks(
  onMessage: (message: { sessionId: string; runEventHighWatermark: number }) => void,
): () => void {
  if (typeof BroadcastChannel === 'undefined') return () => {};
  const channel = new BroadcastChannel(WATERMARK_CHANNEL);
  channel.onmessage = (event) => onMessage(event.data);
  return () => channel.close();
}

export async function clearDeviceLocalState(): Promise<void> {
  localStorage.clear();
  if (typeof indexedDB === 'undefined') return;
  await new Promise<void>((resolve) => {
    const deleteRequest = indexedDB.deleteDatabase(SESSION_CACHE_DB);
    deleteRequest.onsuccess = () => resolve();
    deleteRequest.onerror = () => resolve();
    deleteRequest.onblocked = () => resolve();
  });
}
