'use client';

import { useCallback, useEffect, useRef, useState } from 'react';

type UsePollingOptions<T> = {
  fetcher: () => Promise<T>;
  intervalMs?: number;
  enabled?: boolean;
};

type UsePollingReturn<T> = {
  data: T | null;
  error: string | null;
  isLoading: boolean;
  refresh: () => void;
};

/**
 * Lightweight polling hook for pages that don't have a specific run to
 * subscribe to. Periodically fetches fresh data from a server endpoint.
 */
export function usePolling<T>({
  fetcher,
  intervalMs = 5000,
  enabled = true,
}: UsePollingOptions<T>): UsePollingReturn<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [isLoading, setIsLoading] = useState(false);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const doFetch = useCallback(async () => {
    setIsLoading(true);
    try {
      const result = await fetcher();
      setData(result);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Fetch failed');
    } finally {
      setIsLoading(false);
    }
  }, [fetcher]);

  const refresh = useCallback(() => {
    void doFetch();
  }, [doFetch]);

  useEffect(() => {
    if (!enabled) return;

    void doFetch();

    timerRef.current = setInterval(() => void doFetch(), intervalMs);
    return () => {
      if (timerRef.current) clearInterval(timerRef.current);
    };
  }, [enabled, intervalMs, doFetch]);

  return { data, error, isLoading, refresh };
}
