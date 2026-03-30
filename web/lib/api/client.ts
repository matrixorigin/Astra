import { getRuntimeConfig, getWebConfigurationMessage, type WebDataMode } from '@/lib/runtime-config';

export { getWebConfigurationMessage, type WebDataMode };

export async function getWebDataMode(): Promise<WebDataMode> {
  const config = await getRuntimeConfig();
  return config.mode;
}

export async function apiFetch<T>(path: string): Promise<T> {
  const config = await getRuntimeConfig();

  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error(getWebConfigurationMessage());
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (config.accessToken) {
    headers['Authorization'] = `Bearer ${config.accessToken}`;
  }

  const response = await fetch(new URL(path, config.apiUrl).toString(), {
    headers,
    cache: 'no-store',
  });

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;

    try {
      const errorBody = (await response.json()) as { detail?: string; error?: string };
      detail = errorBody.detail ?? errorBody.error ?? detail;
    } catch {
      // Preserve the HTTP status when the body is not JSON.
    }

    throw new Error(`API request failed for ${path}: ${detail}`);
  }

  return (await response.json()) as T;
}

/**
 * Like apiFetch but returns `null` on 401/403 instead of throwing.
 * Useful for read-only pages that should degrade gracefully without auth.
 */
export async function tryApiFetch<T>(path: string): Promise<T | null> {
  try {
    return await apiFetch<T>(path);
  } catch (err) {
    const msg = err instanceof Error ? err.message : '';
    if (msg.includes('Not authenticated') || msg.includes('401') || msg.includes('403')) {
      return null;
    }
    throw err;
  }
}

export async function apiPost<T>(path: string, body?: unknown): Promise<T> {
  const config = await getRuntimeConfig();

  if (config.mode !== 'live' || !config.apiUrl) {
    throw new Error(getWebConfigurationMessage());
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  };
  if (config.accessToken) {
    headers['Authorization'] = `Bearer ${config.accessToken}`;
  }

  const response = await fetch(new URL(path, config.apiUrl).toString(), {
    method: 'POST',
    headers,
    body: body !== undefined ? JSON.stringify(body) : undefined,
    cache: 'no-store',
  });

  if (!response.ok) {
    let detail = `${response.status} ${response.statusText}`;

    try {
      const errorBody = (await response.json()) as { detail?: string; error?: string };
      detail = errorBody.detail ?? errorBody.error ?? detail;
    } catch {
      // Preserve the HTTP status when the body is not JSON.
    }

    throw new Error(`API request failed for POST ${path}: ${detail}`);
  }

  return (await response.json()) as T;
}
