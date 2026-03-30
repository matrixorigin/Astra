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
