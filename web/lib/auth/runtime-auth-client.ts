export type AuthTokens = {
  access_token: string;
  refresh_token: string;
  token_type?: string;
  expires_in?: number;
};

export type AuthUser = {
  user_id: string;
  username: string;
  email: string;
  display_name: string | null;
};

export type AuthRegisterResponse = AuthTokens & AuthUser;

export type AuthLogoutResponse = {
  message?: string;
};

export type RuntimeAuthResult<T> =
  | { ok: true; data: T }
  | { ok: false; status: number; error: string };

type RuntimeErrorBody = {
  detail?: unknown;
  error?: unknown;
  message?: unknown;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function stringField(value: unknown): string | undefined {
  return typeof value === 'string' && value.trim() ? value : undefined;
}

function hasAuthTokens(value: unknown): value is AuthTokens {
  if (!isRecord(value)) {
    return false;
  }

  return (
    typeof value.access_token === 'string' &&
    value.access_token.length > 0 &&
    typeof value.refresh_token === 'string' &&
    value.refresh_token.length > 0
  );
}

function isAuthRegisterResponse(value: unknown): value is AuthRegisterResponse {
  if (!isRecord(value)) {
    return false;
  }

  const record = value;
  const userId = record['user_id'];
  const username = record['username'];
  const email = record['email'];
  const displayName = record['display_name'];

  return (
    hasAuthTokens(record) &&
    typeof userId === 'string' &&
    typeof username === 'string' &&
    typeof email === 'string' &&
    (typeof displayName === 'string' || displayName === null)
  );
}

function isAuthUser(value: unknown): value is AuthUser {
  if (!isRecord(value)) {
    return false;
  }

  return (
    typeof value.user_id === 'string' &&
    typeof value.username === 'string' &&
    typeof value.email === 'string' &&
    (typeof value.display_name === 'string' || value.display_name === null)
  );
}

function isLogoutResponse(value: unknown): value is AuthLogoutResponse {
  return isRecord(value);
}

async function runtimeErrorFromResponse(
  response: Response,
  defaultMessage: string,
): Promise<string> {
  try {
    const contentType = response.headers.get('content-type') ?? '';
    if (contentType.includes('application/json')) {
      const body = (await response.json()) as RuntimeErrorBody;
      return (
        stringField(body.detail) ??
        stringField(body.error) ??
        stringField(body.message) ??
        `${defaultMessage}: ${response.status} ${response.statusText}`.trim()
      );
    }

    const text = await response.text();
    return text.trim() || `${defaultMessage}: ${response.status} ${response.statusText}`.trim();
  } catch {
    return `${defaultMessage}: ${response.status} ${response.statusText}`.trim();
  }
}

async function decodeRuntimeResponse<T>(
  response: Response,
  defaultError: string,
  guard: (value: unknown) => value is T,
): Promise<RuntimeAuthResult<T>> {
  if (!response.ok) {
    return {
      ok: false,
      status: response.status,
      error: await runtimeErrorFromResponse(response, defaultError),
    };
  }

  let data: unknown;
  try {
    data = await response.json();
  } catch {
    return {
      ok: false,
      status: 502,
      error: `${defaultError}: runtime returned a non-JSON response.`,
    };
  }

  if (!guard(data)) {
    return {
      ok: false,
      status: 502,
      error: `${defaultError}: runtime returned an invalid auth payload.`,
    };
  }

  return { ok: true, data };
}

async function postRuntimeAuth<T>(
  apiUrl: string,
  path: string,
  body: unknown,
  defaultError: string,
  guard: (value: unknown) => value is T,
): Promise<RuntimeAuthResult<T>> {
  try {
    const response = await fetch(new URL(path, apiUrl).toString(), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      cache: 'no-store',
    });
    return decodeRuntimeResponse(response, defaultError, guard);
  } catch (error) {
    return {
      ok: false,
      status: 502,
      error: error instanceof Error ? error.message : 'Cannot reach Astra runtime.',
    };
  }
}

export function runtimeLogin(
  apiUrl: string,
  input: { username: string; password: string },
): Promise<RuntimeAuthResult<AuthTokens>> {
  return postRuntimeAuth(apiUrl, '/auth/login', input, 'Login failed', hasAuthTokens);
}

export function runtimeRegister(
  apiUrl: string,
  input: {
    username: string;
    email: string;
    password: string;
    display_name?: string;
  },
): Promise<RuntimeAuthResult<AuthRegisterResponse>> {
  return postRuntimeAuth(
    apiUrl,
    '/auth/register',
    input,
    'Registration failed',
    isAuthRegisterResponse,
  );
}

export function runtimeRefresh(
  apiUrl: string,
  refreshToken: string,
): Promise<RuntimeAuthResult<AuthTokens>> {
  return postRuntimeAuth(
    apiUrl,
    '/auth/refresh',
    { refresh_token: refreshToken },
    'Token refresh failed',
    hasAuthTokens,
  );
}

export function runtimeLogout(
  apiUrl: string,
  refreshToken: string,
): Promise<RuntimeAuthResult<AuthLogoutResponse>> {
  return postRuntimeAuth(
    apiUrl,
    '/auth/logout',
    { refresh_token: refreshToken },
    'Logout failed',
    isLogoutResponse,
  );
}

export async function runtimeMe(
  apiUrl: string,
  accessToken: string,
): Promise<RuntimeAuthResult<AuthUser>> {
  try {
    const response = await fetch(new URL('/auth/me', apiUrl).toString(), {
      headers: { Authorization: `Bearer ${accessToken}` },
      cache: 'no-store',
    });
    return decodeRuntimeResponse(response, 'Fetch current user failed', isAuthUser);
  } catch (error) {
    return {
      ok: false,
      status: 502,
      error: error instanceof Error ? error.message : 'Cannot reach Astra runtime.',
    };
  }
}
