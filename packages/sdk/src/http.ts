type AstraErrorBody = {
  detail?: unknown;
  error?: unknown;
  message?: unknown;
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

/** Merge `RequestInit.headers` into a plain record. */
export function headersInitToRecord(
  base: Record<string, string>,
  initHeaders?: HeadersInit,
): Record<string, string> {
  if (!initHeaders) {
    return { ...base };
  }
  if (Array.isArray(initHeaders)) {
    const out = { ...base };
    for (const [key, value] of initHeaders) {
      out[key] = value;
    }
    return out;
  }
  if (isHeadersLike(initHeaders)) {
    const out = { ...base };
    initHeaders.forEach((value, key) => {
      out[key] = value;
    });
    return out;
  }
  return { ...base, ...(initHeaders as Record<string, string>) };
}

export function methodCanHaveJson(method: string): boolean {
  const normalized = method.toUpperCase();
  return normalized !== 'GET' && normalized !== 'HEAD';
}

export async function readAstraErrorDetail(response: Response): Promise<string> {
  const statusLine = `${response.status} ${response.statusText}`.trim();
  try {
    const text = await response.text();
    if (!text.trim()) {
      return statusLine;
    }

    const contentType = response.headers.get('content-type') ?? '';
    if (!contentType.includes('application/json')) {
      return text.trim();
    }

    const body = JSON.parse(text) as AstraErrorBody;
    return (
      stringField(body.detail) ??
      stringField(body.error) ??
      stringField(body.message) ??
      statusLine
    );
  } catch {
    return statusLine;
  }
}

export function extractJwtSubject(token: string): string | null {
  try {
    const payloadSegment = token.split('.')[1];
    if (!payloadSegment) {
      return null;
    }
    const normalized = payloadSegment.replace(/-/g, '+').replace(/_/g, '/');
    const padded = normalized.padEnd(normalized.length + ((4 - (normalized.length % 4)) % 4), '=');
    const decoded = typeof atob === 'function'
      ? atob(padded)
      : Buffer.from(padded, 'base64').toString('utf8');
    const payload = JSON.parse(decoded) as {
      sub?: unknown;
    };
    return typeof payload.sub === 'string' && payload.sub ? payload.sub : null;
  } catch {
    return null;
  }
}
