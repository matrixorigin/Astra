import { WebApiError } from '@/lib/api/errors';

export type RequestJsonInit = RequestInit & { timeoutMs?: number };

export async function requestJson<T>(path: string, init: RequestJsonInit = {}): Promise<T> {
  const { timeoutMs = 0, signal: externalSignal, ...requestInit } = init;
  const controller = new AbortController();
  const abortFromCaller = () => controller.abort();
  if (externalSignal?.aborted) {
    controller.abort();
  } else {
    externalSignal?.addEventListener('abort', abortFromCaller, { once: true });
  }
  const timeout = Number.isFinite(timeoutMs) && timeoutMs > 0
    ? setTimeout(() => controller.abort(), timeoutMs)
    : undefined;

  try {
    const response = await fetch(path, {
      ...requestInit,
      signal: controller.signal,
      headers: {
        'Content-Type': 'application/json',
        ...(requestInit.headers ?? {}),
      },
    });

    if (!response.ok) {
      let detail = `${response.status} ${response.statusText}`;
      try {
        const body = (await response.json()) as { error?: string; detail?: string };
        detail = body.error ?? body.detail ?? detail;
      } catch {
        // Preserve the HTTP status.
      }
      throw new WebApiError(response.status, detail);
    }

    return (await response.json()) as T;
  } finally {
    if (timeout !== undefined) clearTimeout(timeout);
    externalSignal?.removeEventListener('abort', abortFromCaller);
  }
}

export function toQuery(params: Record<string, string | number | boolean | null | undefined>) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null && `${value}`.length > 0) {
      query.set(key, `${value}`);
    }
  }
  const text = query.toString();
  return text ? `?${text}` : '';
}
