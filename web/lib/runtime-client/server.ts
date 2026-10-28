import {
  AstraClient,
  extractJwtSubject,
  headersInitToRecord,
  methodCanHaveJson,
} from "@astra/sdk";
import {
  getRuntimeConfig,
  getWebConfigurationMessage,
  type RuntimeConfig,
} from "@/lib/runtime-config";
import { RuntimeClientError, readRuntimeErrorDetail } from "./errors";

export type RuntimeAuthMode = "required" | "optional" | "none";

export type RuntimeRequestInit = Omit<RequestInit, "body"> & {
  auth?: RuntimeAuthMode;
  retryUnauthorized?: boolean;
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

  constructor(config: RuntimeConfig) {
    if (config.mode !== "live" || !config.apiUrl) {
      throw new RuntimeClientError({
        operation: "initialize runtime client",
        path: "/",
        status: 503,
        detail: getWebConfigurationMessage(),
      });
    }

    this.config = config;
    this.sdk = new AstraClient({
      baseUrl: new URL("/", config.apiUrl).toString(),
      accessToken: config.accessToken,
      refreshToken: config.refreshToken,
    });
  }

  async get<T>(path: string, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: "GET" });
  }

  async post<T>(
    path: string,
    json?: unknown,
    init?: RuntimeRequestInit,
  ): Promise<T> {
    return this.request<T>(path, { ...init, method: "POST", json });
  }

  async put<T>(
    path: string,
    json?: unknown,
    init?: RuntimeRequestInit,
  ): Promise<T> {
    return this.request<T>(path, { ...init, method: "PUT", json });
  }

  async delete<T>(path: string, init?: RuntimeRequestInit): Promise<T> {
    return this.request<T>(path, { ...init, method: "DELETE" });
  }

  async request<T>(path: string, init: RuntimeRequestInit = {}): Promise<T> {
    const response = await this.fetchResponse(path, { ...init, retryUnauthorized: init.retryUnauthorized ?? true });
    if (!response.ok) {
      throw new RuntimeClientError({
        operation: init.operation ?? `${init.method ?? "GET"} ${path}`,
        path,
        status: response.status,
        detail: await readRuntimeErrorDetail(response),
      });
    }
    if (
      response.status === 204 ||
      response.headers.get("content-length") === "0"
    ) {
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
        operation: init.operation ?? `${init.method ?? "GET"} ${path}`,
        path,
        status: 502,
        detail: `Runtime returned invalid JSON for ${path}.`,
        cause: error,
      });
    }
  }

  async fetchResponse(
    path: string,
    init: RuntimeRequestInit = {},
  ): Promise<Response> {
    const operation = init.operation ?? `${init.method ?? "GET"} ${path}`;
    const auth = init.auth ?? "optional";
    return this.sdk.fetchResponse(
      path,
      this.toRequestInit(init, auth, operation, path),
      { auth: auth === "none" ? "none" : "optional", retryUnauthorized: init.retryUnauthorized },
    );
  }

  private toRequestInit(
    init: RuntimeRequestInit,
    auth: RuntimeAuthMode,
    operation: string,
    path: string,
  ): RequestInit {
    const method = (init.method ?? "GET").toUpperCase();
    const body =
      init.json !== undefined ? JSON.stringify(init.json) : init.body;
    const headers = headersInitToRecord(
      this.baseAuthHeaders(auth, {
        hasJsonBody: init.json !== undefined,
        method,
      }),
      init.headers,
    );

    if (auth === "required" && !this.sdk.getTokens().accessToken) {
      throw new RuntimeClientError({
        operation,
        path,
        status: 401,
        detail: "Runtime authentication is missing.",
      });
    }

    const { auth: _auth, json: _json, operation: _operation, retryUnauthorized: _retryUnauthorized, ...rest } = init;
    return {
      ...rest,
      method,
      headers,
      body,
      cache: init.cache ?? "no-store",
    };
  }

  private baseAuthHeaders(
    auth: RuntimeAuthMode,
    request?: { hasJsonBody: boolean; method: string },
  ): Record<string, string> {
    const headers: Record<string, string> = {};
    if (request?.hasJsonBody && methodCanHaveJson(request.method)) {
      headers["Content-Type"] = "application/json";
    }
    const { accessToken } = this.sdk.getTokens();
    if (auth !== "none" && accessToken) {
      const userId = extractJwtSubject(accessToken);
      if (userId) {
        headers["X-User-Id"] = userId;
      }
    }
    return headers;
  }


}

export async function getRuntimeClient(
  options: RuntimeClientOptions = {},
): Promise<WebRuntimeClient | null> {
  const config = await getRuntimeConfig();
  if (config.mode !== "live" || !config.apiUrl) {
    return null;
  }
  if (options.auth === "required" && !config.accessToken) {
    return null;
  }
  return new WebRuntimeClient(config);
}

export async function requireRuntimeClient(
  options: RuntimeClientOptions = {},
): Promise<WebRuntimeClient> {
  const config = await getRuntimeConfig();
  const auth = options.auth ?? "optional";
  if (config.mode !== "live" || !config.apiUrl) {
    throw new RuntimeClientError({
      operation: options.operation ?? "initialize runtime client",
      path: "/",
      status: 503,
      detail: config.message || getWebConfigurationMessage(),
    });
  }
  if (auth === "required" && !config.accessToken) {
    throw new RuntimeClientError({
      operation: options.operation ?? "initialize runtime client",
      path: "/",
      status: 401,
      detail: "Runtime authentication is missing.",
    });
  }
  return new WebRuntimeClient(config);
}
