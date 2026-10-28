import {
  PATH_AUTH_LOGIN,
  PATH_AUTH_REGISTER,
  type AuthResult,
  type UserInfo,
} from "@astra/sdk";
import { WebRuntimeClient } from "@/lib/runtime-client";
import { jwtSessionHint } from "@/lib/auth/session-cookie";

export type AuthTokens = AuthResult;
export type AuthUser = Omit<UserInfo, "display_name"> & {
  display_name: string | null;
};

export type AuthRegisterResponse = AuthTokens & AuthUser;

export type RuntimeAuthResult<T> =
  | { ok: true; data: T }
  | { ok: false; status: number; error: string };

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function stringField(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function hasAuthTokens(value: unknown): value is AuthTokens {
  if (!isRecord(value)) {
    return false;
  }

  if (typeof value.access_token !== "string" || typeof value.refresh_token !== "string") return false;
  const session = jwtSessionHint(value.refresh_token);
  return session !== undefined && jwtSessionHint(value.access_token) === session;
}

export function isAuthUser(value: unknown): value is AuthUser {
  return (
    isRecord(value) &&
    typeof value.user_id === "string" &&
    value.user_id.trim().length > 0 &&
    typeof value.username === "string" &&
    typeof value.email === "string" &&
    (typeof value.display_name === "string" || value.display_name === null)
  );
}

function isAuthRegisterResponse(value: unknown): value is AuthRegisterResponse {
  return isAuthUser(value) && hasAuthTokens(value);
}

function runtimeClientForAuth(
  apiUrl: string,
  tokens?: { accessToken?: string; refreshToken?: string },
) {
  return new WebRuntimeClient({
    mode: "live",
    source: "cookie",
    apiUrl,
    accessToken: tokens?.accessToken,
    refreshToken: tokens?.refreshToken,
    demoMode: false,
    hasAccessToken: Boolean(tokens?.accessToken),
    hasRefreshToken: Boolean(tokens?.refreshToken),
    message: tokens?.accessToken
      ? `Connected to ${apiUrl} with authentication.`
      : `Connected to ${apiUrl} without authentication.`,
  });
}

async function runtimeErrorFromResponse(
  response: Response,
  defaultMessage: string,
): Promise<string> {
  try {
    const contentType = response.headers.get("content-type") ?? "";
    if (contentType.includes("application/json")) {
      const body = (await response.json()) as {
        detail?: unknown;
        error?: unknown;
        message?: unknown;
      };
      return (
        stringField(body.detail) ??
        stringField(body.error) ??
        stringField(body.message) ??
        `${defaultMessage}: ${response.status} ${response.statusText}`.trim()
      );
    }

    const text = await response.text();
    return (
      text.trim() ||
      `${defaultMessage}: ${response.status} ${response.statusText}`.trim()
    );
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
    const client = runtimeClientForAuth(apiUrl);
    const response = await client.fetchResponse(path, {
      method: "POST",
      redirect: "error",
      auth: "none",
      json: body,
      operation: defaultError,
    });
    return decodeRuntimeResponse(response, defaultError, guard);
  } catch (error) {
    return {
      ok: false,
      status: 502,
      error:
        error instanceof Error ? error.message : "Cannot reach Astra runtime.",
    };
  }
}

export function runtimeLogin(
  apiUrl: string,
  input: { username: string; password: string },
): Promise<RuntimeAuthResult<AuthTokens>> {
  return postRuntimeAuth(
    apiUrl,
    PATH_AUTH_LOGIN,
    input,
    "Login failed",
    hasAuthTokens,
  );
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
    PATH_AUTH_REGISTER,
    input,
    "Registration failed",
    isAuthRegisterResponse,
  );
}
