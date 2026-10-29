import { randomBytes } from "node:crypto";
import { cookies } from "next/headers";
import {
  RuntimeClientError,
  readRuntimeErrorDetail,
} from "@/lib/runtime-client/errors";
import {
  SESSION_COOKIE,
  SESSION_MAX_AGE,
  parseSelection,
  jwtSessionHint,
  jwtIdentityHint,
  runtimeRoot,
  selectionCookie,
  sessionCookieOptions,
  type SessionSelection,
} from "./session-cookie";

export async function selectedSession(): Promise<SessionSelection | undefined> {
  const value = (await cookies()).get(SESSION_COOKIE)?.value;
  if (value === undefined) return undefined;
  try {
    return parseSelection(value);
  } catch {
    throw new RuntimeClientError({
      operation: "read browser session",
      path: "/",
      status: 401,
      detail: "Invalid session; sign in again.",
    });
  }
}

/** Invalid selection can be replaced explicitly; it never authorizes a request. */
export async function replaceableSession(): Promise<
  SessionSelection | undefined
> {
  try {
    return await selectedSession();
  } catch (error) {
    if (error instanceof RuntimeClientError && error.status === 401)
      return undefined;
    throw error;
  }
}

async function sessionRequest(
  apiUrl: string,
  path: string,
  body: unknown,
): Promise<unknown> {
  let response: Response;
  try {
    response = await fetch(new URL(path, apiUrl), {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
      cache: "no-store",
      signal: AbortSignal.timeout(30_000),
      redirect: "error",
    });
  } catch {
    throw new RuntimeClientError({
      operation: "browser session",
      path,
      status: 503,
      detail: "Cannot reach the selected runtime. Please retry.",
    });
  }
  if (!response.ok) {
    throw new RuntimeClientError({
      operation: "browser session",
      path,
      status: response.status,
      detail: await readRuntimeErrorDetail(response),
    });
  }
  try {
    return await response.json();
  } catch {
    throw new RuntimeClientError({
      operation: "browser session",
      path,
      status: 502,
      detail: "Invalid session response from runtime.",
    });
  }
}

export async function resolveBrowserSession(
  selection: Extract<SessionSelection, { mode: "browser" }>,
): Promise<string> {
  const data = await sessionRequest(selection.apiUrl, "/auth/refresh", {
    refresh_token: selection.handle,
  });
  const tokens = data as {
    access_token?: unknown;
    refresh_token?: unknown;
  } | null;
  if (
    !tokens ||
    typeof tokens.access_token !== "string" ||
    jwtSessionHint(tokens.access_token) !== selection.sessionId ||
    tokens.refresh_token !== selection.handle
  ) {
    throw new RuntimeClientError({
      operation: "resolve browser session",
      path: "/auth/refresh",
      status: 502,
      detail: "Invalid browser credential response from runtime.",
    });
  }
  return tokens.access_token;
}

export async function revokeSelection(
  selection?: SessionSelection,
): Promise<void> {
  if (selection?.mode === "browser") {
    await sessionRequest(selection.apiUrl, "/auth/logout", {
      refresh_token: selection.handle,
    });
  }
}

/** Call only at an explicit login/import boundary, never during rendering. */
export async function adoptBrowserSession(
  apiUrl: string,
  tokens: { access_token: string; refresh_token: string },
  proof = randomBytes(32).toString("base64url"),
): Promise<string> {
  const root = runtimeRoot(apiUrl);
  const identity = jwtIdentityHint(tokens.refresh_token);
  if (!identity) throw new RuntimeClientError({ operation: "create browser session", path: "/auth/browser-sessions", status: 400, detail: "Invalid session credential." });
  const { userId, sessionId } = identity;
  const handle = `bs3.${Buffer.from(root).toString("base64url")}.${Buffer.from(userId).toString("base64url")}.${Buffer.from(sessionId).toString("base64url")}.${proof}`;
  parseSelection(handle);
  try {
    const created = await sessionRequest(root, "/auth/browser-sessions", {
      handle,
      access_token: tokens.access_token,
      refresh_token: tokens.refresh_token,
    });
    if (!created || typeof created !== "object" || !("expires_in" in created) ||
      typeof created.expires_in !== "number" || !Number.isInteger(created.expires_in) ||
      created.expires_in <= 0 || created.expires_in > SESSION_MAX_AGE) {
      throw new RuntimeClientError({ operation: "create browser session", path: "/auth/browser-sessions", status: 502, detail: "Invalid session response from runtime." });
    }
  } catch (error) {
    // A lost creation response does not imply rollback. Only our independently
    // generated handle can recover a committed owner; never replay a JWT.
    try {
      await resolveBrowserSession({ mode: "browser", apiUrl: root, sessionId, handle });
    } catch {
      throw error;
    }
  }
  return handle;
}

export async function saveSelection(value: string): Promise<void> {
  parseSelection(value);
  (await cookies()).set(SESSION_COOKIE, value, sessionCookieOptions());
}

export async function logoutSelection(): Promise<void> {
  const selection = await selectedSession();
  await revokeSelection(selection);
  // Handle logout has no Set-Cookie: a delayed response must not erase a new
  // login. Static tokens cannot be revoked by Web; clear their local selection.
  if (selection?.mode !== "browser") {
    const apiUrl =
      selection?.mode === "live"
        ? selection.apiUrl
        : runtimeRoot(process.env.ASTRA_API_URL ?? "http://localhost:17001");
    await saveSelection(
      selectionCookie({ mode: "live", apiUrl, auth: "none" }),
    );
  }
}
