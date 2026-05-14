import { cookies } from 'next/headers';
import {
  AstraClient,
  PATH_AUTH_REFRESH,
  extractJwtSubject,
  headersInitToRecord,
  joinApiPath,
  methodCanHaveJson,
} from '@astra/sdk';
import {
  ACCESS_TOKEN_COOKIE,
  API_URL_COOKIE,
  DEFAULT_API_URL,
  REFRESH_TOKEN_COOKIE,
  getRuntimeConfig,
  getWebConfigurationMessage,
  type RuntimeConfig,
} from '@/lib/runtime-config';
import { RuntimeClientError, readRuntimeErrorDetail } from './errors';

export type RuntimeAuthMode = 'required' | 'optional' | 'none';

export type RuntimeRequestInit = Omit<RequestInit, 'body'> & {
  auth?: RuntimeAuthMode;
  json?: unknown;
  operation?: string;
  body?: BodyInit | null;
};

type RuntimeClientOptions = {
  auth?: RuntimeAuthMode;
  operation?: string;
};

export class WebRuntimeClient {
  readonly config: RuntimeConfig;
  readonly sdk: AstraClient;
  private accessToken?: string;
  private refreshToken?: string;

  constructor(config: RuntimeConfig) {
    if (config.mode !== 'live' || !config.apiUrl) {
      throw new RuntimeClientError({
        operation: 'initialize runtime client',
        path: '/',
        status: 503,
        detail: getWebConfigurationMessage(),
      });
    }

    this.config = config;
    this.accessToken = config.accessToken;
    this.refreshToken = config.refreshToken;
    this.sdk = new AstraClient({
      baseUrl: config.apiUrl,
      accessToken: config.accessToken,
      refreshToken: config.refreshToken,
      headers: this.baseAuthHeaders('optional'),
      onTokenRefresh: async (tokens) => {
        this.accessToken = tokens.accessToken;
        this.refreshToken = tokens.refreshToken;
        await this.persistTokens(tokens.accessToken, tokens.refreshToken);
      },
    });
  }

  get apiUrl(): string {
    return this.config.apiUrl ?? DEFAULT_API_URL;
  }

  async get<T>(path: string, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: 'GET' });
  }

  async post<T>(path: string, json?: unknown, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: 'POST', json });
  }

  async put<T>(path: string, json?: unknown, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: 'PUT', json });
  }

  async delete<T>(path: string, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: 'DELETE' });
  }

  async request<T>(path: string, init: RuntimeRequestInit = {}): Promise<T> {
    const response = await this.fetchResponse(path, init);
    if (!response.ok) {
      throw new RuntimeClientError({
        operation: init.operation ?? `${init.method ?? 'GET'} ${path}`,
        path,
        status: response.status,
        detail: await readRuntimeErrorDetail(response),
      });
    }
    if (response.status === 204 || response.headers.get('content-length') === '0') {
      return undefined as T;
    }
    const text = await response.text();
    if (!text) {
      return undefined as T;
    }
    try {
      return JSON.parse(text) as T;
    } catch (error) {
      throw new RuntimeClientError({
        operation: init.operation ?? `${init.method ?? 'GET'} ${path}`,
        path,
        status: response.status,
        detail: `Runtime returned invalid JSON for ${path}.`,
        cause: error,
      });
    }
  }

  async fetchResponse(path: string, init: RuntimeRequestInit = {}): Promise<Response> {
    const operation = init.operation ?? `${init.method ?? 'GET'} ${path}`;
    const auth = init.auth ?? 'optional';
    const url = this.url(path);
    const request = this.toRequestInit(init, auth, operation, path);

    let response = await fetch(url, request);
    if (response.status === 401 && auth !== 'none' && this.refreshToken) {
      const refreshed = await this.refreshTokens();
      if (refreshed) {
        response = await fetch(url, this.toRequestInit(init, auth, operation, path));
      }
    }
    return response;
  }

  url(path: string): string {
    return new URL(joinApiPath(undefined, path), this.apiUrl).toString();
  }

  private toRequestInit(
    init: RuntimeRequestInit,
    auth: RuntimeAuthMode,
    operation: string,
    path: string,
  ): RequestInit {
    const method = (init.method ?? 'GET').toUpperCase();
    const body = init.json !== undefined ? JSON.stringify(init.json) : init.body;
    const headers = headersInitToRecord(
      this.baseAuthHeaders(auth, { hasJsonBody: init.json !== undefined, method }),
      init.headers,
    );

    if (auth === 'required' && !this.accessToken) {
      throw new RuntimeClientError({
        operation,
        path,
        status: 401,
        detail: 'Runtime authentication is missing.',
      });
    }

    const { auth: _auth, json: _json, operation: _operation, ...rest } = init;
    return {
      ...rest,
      method,
      headers,
      body,
      cache: init.cache ?? 'no-store',
    };
  }

  private baseAuthHeaders(
    auth: RuntimeAuthMode,
    request?: { hasJsonBody: boolean; method: string },
  ): Record<string, string> {
    const headers: Record<string, string> = {};
    if (request?.hasJsonBody && methodCanHaveJson(request.method)) {
      headers['Content-Type'] = 'application/json';
    }
    if (auth !== 'none' && this.accessToken) {
      headers.Authorization = `Bearer ${this.accessToken}`;
      const userId = extractJwtSubject(this.accessToken);
      if (userId) {
        headers['X-User-Id'] = userId;
      }
    }
    return headers;
  }

  private async refreshTokens(): Promise<boolean> {
    if (!this.refreshToken) {
      return false;
    }

    const response = await fetch(this.url(PATH_AUTH_REFRESH), {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ refresh_token: this.refreshToken }),
      cache: 'no-store',
    });
    if (!response.ok) {
      return false;
    }

    const payload = (await response.json()) as {
      access_token?: unknown;
      refresh_token?: unknown;
    };
    if (typeof payload.access_token !== 'string' || typeof payload.refresh_token !== 'string') {
      return false;
    }

    this.accessToken = payload.access_token;
    this.refreshToken = payload.refresh_token;
    this.sdk.setTokens(payload.access_token, payload.refresh_token);
    await this.persistTokens(payload.access_token, payload.refresh_token);
    return true;
  }

  private async persistTokens(accessToken: string, refreshToken: string): Promise<void> {
    const cookieStore = await cookies();
    cookieStore.set(ACCESS_TOKEN_COOKIE, accessToken, {
      httpOnly: true,
      sameSite: 'lax',
      path: '/',
    });
    cookieStore.set(REFRESH_TOKEN_COOKIE, refreshToken, {
      httpOnly: true,
      sameSite: 'lax',
      path: '/',
    });
  }
}

export async function getRuntimeClient(
  options: RuntimeClientOptions = {},
): Promise<WebRuntimeClient | null> {
  const config = await getRuntimeConfig();
  if (config.mode !== 'live' || !config.apiUrl) {
    return null;
  }
  if (options.auth === 'required' && !config.accessToken) {
    return null;
  }
  return new WebRuntimeClient(config);
}

export async function requireRuntimeClient(
  options: RuntimeClientOptions = {},
): Promise<WebRuntimeClient> {
  const config = await getRuntimeConfig();
  const auth = options.auth ?? 'optional';
  if (config.mode !== 'live' || !config.apiUrl) {
    throw new RuntimeClientError({
      operation: options.operation ?? 'initialize runtime client',
      path: '/',
      status: 503,
      detail: config.message || getWebConfigurationMessage(),
    });
  }
  if (auth === 'required' && !config.accessToken) {
    throw new RuntimeClientError({
      operation: options.operation ?? 'initialize runtime client',
      path: '/',
      status: 401,
      detail: 'Runtime authentication is missing.',
    });
  }
  return new WebRuntimeClient(config);
}

export async function getRuntimeClientFromCookies(
  options: RuntimeClientOptions = {},
): Promise<WebRuntimeClient> {
  const cookieStore = await cookies();
  const apiUrl = cookieStore.get(API_URL_COOKIE)?.value ?? process.env.ASTRA_API_URL ?? DEFAULT_API_URL;
  const accessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value ?? process.env.ASTRA_ACCESS_TOKEN;
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;
  if (options.auth === 'required' && !accessToken) {
    throw new RuntimeClientError({
      operation: options.operation ?? 'initialize runtime client',
      path: '/',
      status: 401,
      detail: 'Runtime authentication is missing.',
    });
  }
  return new WebRuntimeClient({
    mode: 'live',
    source: cookieStore.get(API_URL_COOKIE)?.value ? 'cookie' : 'env',
    apiUrl,
    accessToken,
    refreshToken,
    demoMode: false,
    hasAccessToken: Boolean(accessToken),
    hasRefreshToken: Boolean(refreshToken),
    message: accessToken
      ? `Connected to ${apiUrl} with authentication.`
      : `Connected to ${apiUrl} without authentication. Login for full access.`,
  });
}
