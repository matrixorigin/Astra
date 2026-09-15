"use client";

import { useEffect, useRef } from "react";

type UseVisiblePollOptions = {
  enabled: boolean;
  intervalMs: number;
  maximumIntervalMs: number;
  immediate?: boolean;
  refresh: () => Promise<boolean>;
};

/** Run one sequential, jittered poll loop while the document is visible. */
export function useVisiblePoll({
  enabled,
  intervalMs,
  maximumIntervalMs,
  immediate = false,
  refresh,
}: UseVisiblePollOptions) {
  const refreshRef = useRef(refresh);

  useEffect(() => {
    refreshRef.current = refresh;
  }, [refresh]);

  useEffect(() => {
    if (!enabled) return;

    let cancelled = false;
    let inFlight = false;
    let failures = 0;
    let timer: number | null = null;

    const clearTimer = () => {
      if (timer !== null) {
        window.clearTimeout(timer);
        timer = null;
      }
    };

    const jitter = (delay: number) =>
      Math.max(250, Math.round(delay * (0.8 + Math.random() * 0.4)));

    const schedule = (delay: number) => {
      clearTimer();
      if (cancelled || document.visibilityState !== "visible") return;
      timer = window.setTimeout(() => {
        timer = null;
        void run();
      }, jitter(delay));
    };

    async function run() {
      if (
        cancelled ||
        inFlight ||
        document.visibilityState !== "visible"
      ) {
        return;
      }
      inFlight = true;
      let succeeded = false;
      try {
        succeeded = await refreshRef.current();
      } catch {
        succeeded = false;
      } finally {
        inFlight = false;
      }
      if (cancelled) return;
      if (succeeded) {
        failures = 0;
        schedule(intervalMs);
      } else {
        failures = Math.min(failures + 1, 6);
        schedule(Math.min(maximumIntervalMs, intervalMs * 2 ** failures));
      }
    }

    const onVisibilityChange = () => {
      clearTimer();
      if (document.visibilityState === "visible") void run();
    };

    document.addEventListener("visibilitychange", onVisibilityChange);
    if (immediate) {
      void run();
    } else {
      schedule(intervalMs);
    }

    return () => {
      cancelled = true;
      clearTimer();
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [enabled, immediate, intervalMs, maximumIntervalMs]);
}
