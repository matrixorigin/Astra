type AstraErrorBody = {
  detail?: unknown;
  error?: unknown;
  message?: unknown;
  code?: unknown;
  category?: unknown;
  retryable?: unknown;
  action_hints?: unknown;
};

export type DecodedAstraError = {
  detail: string;
  code?: string;
  category?: string;
  retryable?: boolean;
  actionHints?: string[];
};

function stringField(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value : undefined;
}

/** `Headers` or undici/VM instances where `instanceof Headers` is unreliable. */
export function isHeadersLike(headers: unknown): headers is Headers {
  if (!headers || typeof headers !== 'object' || Array.isArray(headers)) {
    return false;
  }
  return (
    headers instanceof Headers ||
    (
      'forEach' in headers &&
      typeof (headers as Headers).forEach === 'function'
    )
  );
}

/** Merge headers case-insensitively; later values replace earlier values. */
export function headersInitToRecord(
  base: Record<string, string>,
  initHeaders?: HeadersInit,
): Record<string, string> {
  const out: Record<string, string> = {};
  const names = new Map<string, string>();
  const set = (key: string, value: string) => {
    const name = key.toLowerCase();
    const previous = names.get(name);
    if (previous !== undefined) delete out[previous];
    out[key] = value;
    names.set(name, key);
  };
  Object.entries(base).forEach(([key, value]) => set(key, value));
  if (!initHeaders) return out;
  if (Array.isArray(initHeaders)) {
    for (const [key, value] of initHeaders) {
      set(key, value);
    }
    return out;
  }
  if (isHeadersLike(initHeaders)) {
    initHeaders.forEach((value, key) => set(key, value));
    return out;
  }
  Object.entries(initHeaders).forEach(([key, value]) => set(key, value));
  return out;
}

export function methodCanHaveJson(method: string): boolean {
  const normalized = method.toUpperCase();
  return normalized !== 'GET' && normalized !== 'HEAD';
}

export async function readAstraError(response: Response): Promise<DecodedAstraError> {
  const statusLine = `${response.status} ${response.statusText}`.trim();
  try {
    const text = await response.text();
    if (!text.trim()) {
      return { detail: statusLine };
    }

    const contentType = response.headers.get('content-type') ?? '';
    if (!contentType.includes('application/json')) {
      return { detail: text.trim() };
    }

    const body = JSON.parse(text) as AstraErrorBody;
    const actionHints = Array.isArray(body.action_hints)
      ? body.action_hints.filter((value): value is string => typeof value === 'string')
      : undefined;
    return {
      detail:
        stringField(body.detail) ??
        stringField(body.error) ??
        stringField(body.message) ??
        stringField(body.code) ??
        statusLine,
      ...(stringField(body.code) ? { code: stringField(body.code) } : {}),
      ...(stringField(body.category) ? { category: stringField(body.category) } : {}),
      ...(typeof body.retryable === 'boolean' ? { retryable: body.retryable } : {}),
      ...(actionHints ? { actionHints } : {}),
    };
  } catch {
    return { detail: statusLine };
  }
}

export async function readAstraErrorDetail(response: Response): Promise<string> {
  return (await readAstraError(response)).detail;
}

export function extractJwtSubject(token: string): string | null {
  try {
    const payloadSegment = token.split('.')[1];
    if (!payloadSegment) {
      return null;
    }
    const normalized = payloadSegment.replace(/-/g, '+').replace(/_/g, '/');
    const padded = normalized.padEnd(normalized.length + ((4 - (normalized.length % 4)) % 4), '=');
    const bytes = typeof atob === 'function'
      ? Uint8Array.from(atob(padded), (character) => character.charCodeAt(0))
      : Buffer.from(padded, 'base64');
    const decoded = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    const payload = JSON.parse(decoded) as {
      sub?: unknown;
    };
    return typeof payload.sub === 'string' && payload.sub ? payload.sub : null;
  } catch {
    return null;
  }
}
