/**
 * Model list cache with TTL-based expiry and periodic eviction.
 *
 * Replaces a bare Map<string, {promise, expiresAt}> with an encapsulated
 * service that supports size-bounded growth and proactive cleanup.
 */
const DEFAULT_TTL_MS = 60_000;
const MAX_ENTRIES = 100;
const EVICT_INTERVAL_MS = 300_000; // 5 minutes

type CacheEntry<T> = {
  promise: Promise<T>;
  expiresAt: number;
};

export class ModelCacheService<T = unknown> {
  #store = new Map<string, CacheEntry<T>>();
  #ttlMs: number;
  #maxEntries: number;
  #evictTimer: ReturnType<typeof setInterval> | null = null;

  constructor(opts?: { ttlMs?: number; maxEntries?: number }) {
    this.#ttlMs = opts?.ttlMs ?? DEFAULT_TTL_MS;
    this.#maxEntries = opts?.maxEntries ?? MAX_ENTRIES;
  }

  /** Start periodic eviction. Call when the service should be long-lived. */
  startEviction(intervalMs = EVICT_INTERVAL_MS): void {
    if (this.#evictTimer) return;
    this.#evictTimer = setInterval(() => this.evictExpired(), intervalMs);
    // Allow the timer to not keep the process alive (Node.js).
    if (typeof this.#evictTimer === "object" && "unref" in this.#evictTimer) {
      (this.#evictTimer as NodeJS.Timeout).unref();
    }
  }

  /** Stop periodic eviction. */
  stopEviction(): void {
    if (this.#evictTimer) {
      clearInterval(this.#evictTimer);
      this.#evictTimer = null;
    }
  }

  /** Look up a cached promise, returning null if missing or expired. */
  get(key: string): Promise<T> | null {
    const entry = this.#store.get(key);
    if (!entry) return null;
    if (entry.expiresAt <= Date.now()) {
      this.#store.delete(key);
      return null;
    }
    return entry.promise;
  }

  /** Store a promise with TTL-based expiry. */
  set(key: string, promise: Promise<T>, ttlMs?: number): void {
    const effectiveTtl = ttlMs ?? this.#ttlMs;
    // Evict oldest entry if at capacity (simple FIFO — first key iteration).
    if (this.#store.size >= this.#maxEntries) {
      const firstKey = this.#store.keys().next().value;
      if (firstKey !== undefined) {
        this.#store.delete(firstKey);
      }
    }
    this.#store.set(key, {
      promise,
      expiresAt: Date.now() + effectiveTtl,
    });
  }

  /** Remove a specific entry. */
  invalidate(key: string): void {
    this.#store.delete(key);
  }

  /** Remove all expired entries. */
  evictExpired(): number {
    const now = Date.now();
    let removed = 0;
    for (const [key, entry] of this.#store) {
      if (entry.expiresAt <= now) {
        this.#store.delete(key);
        removed++;
      }
    }
    return removed;
  }

  /** Number of entries (including expired ones not yet evicted). */
  get size(): number {
    return this.#store.size;
  }

  /** Remove all entries. */
  clear(): void {
    this.#store.clear();
  }

  /** Release timers and entries owned by this cache instance. */
  dispose(): void {
    this.stopEviction();
    this.clear();
  }
}

export function createModelCacheService<T = unknown>(opts?: {
  ttlMs?: number;
  maxEntries?: number;
}) {
  return new ModelCacheService<T>(opts);
}

/** Singleton model list cache used by requireKnownBackendModelName. */
export const modelCache = new ModelCacheService<
  Array<{ model_id?: string | null; name?: string | null }>
>({ ttlMs: DEFAULT_TTL_MS, maxEntries: MAX_ENTRIES });

export function resetModelCacheForTests() {
  modelCache.dispose();
}
