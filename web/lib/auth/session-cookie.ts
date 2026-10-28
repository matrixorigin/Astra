/** A single cookie binds the selected runtime and its authentication mode. */
export const SESSION_COOKIE = "astra_session";
export const SESSION_MAX_AGE = 365 * 24 * 60 * 60;
const MAX_COOKIE_BYTES = 3800;

export type SessionSelection =
  | { mode: "browser"; apiUrl: string; sessionId: string; handle: string }
  | { mode: "live"; apiUrl: string; auth: "none" }
  | { mode: "live"; apiUrl: string; auth: "static"; accessToken: string }
  | { mode: "demo"; apiUrl?: string }
  | { mode: "default" };

export function runtimeRoot(value: string): string {
  const url = new URL(value);
  if (
    value.length > 1024 ||
    !["http:", "https:"].includes(url.protocol) ||
    !url.hostname ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  ) {
    throw new Error(
      "Runtime URL must be HTTP(S), without credentials, query or fragment.",
    );
  }
  return new URL("/", url).toString();
}

function encode(value: string): string {
  return Buffer.from(value, "utf8").toString("base64url");
}
function decode(value: string): string {
  const decoded = Buffer.from(value, "base64url");
  if (decoded.toString("base64url") !== value)
    throw new Error("Invalid session selection.");
  return decoded.toString("utf8");
}

/** Untrusted JWT hints; the runtime verifies the signed identity. */
export function jwtIdentityHint(token: string): { userId: string; sessionId: string } | undefined {
  try {
    const parts = token.split(".");
    if (parts.length !== 3 || parts.some((part) => !part)) return undefined;
    const payload = JSON.parse(decode(parts[1]));
    const { sub, sid } = payload;
    if (typeof sub !== "string" || !sub || Buffer.byteLength(sub, "utf8") > 128 ||
        typeof sid !== "string" || !sid || Buffer.byteLength(sid, "utf8") > 64) return undefined;
    return { userId: sub, sessionId: sid };
  } catch { return undefined; }
}
export function jwtSessionHint(token: string): string | undefined {
  try {
    const parts = token.split(".");
    if (parts.length !== 3 || parts.some((part) => !part)) return undefined;
    const sid: unknown = JSON.parse(decode(parts[1]))?.sid;
    return typeof sid === "string" && sid.length > 0 && Buffer.byteLength(sid, "utf8") <= 64 ? sid : undefined;
  } catch { return undefined; }
}

export function parseSelection(value: string): SessionSelection {
  if (value.length > MAX_COOKIE_BYTES)
    throw new Error("Session selection is too large.");
  if (value.startsWith("bs3.")) {
    const parts = value.split(".");
    const apiUrl = decode(parts[1] ?? "");
    const userId = decode(parts[2] ?? "");
    const sessionId = decode(parts[3] ?? "");
    if (
      value.length > 2048 ||
      parts.length !== 5 ||
      !userId || Buffer.byteLength(userId, "utf8") > 128 || encode(userId) !== parts[2] ||
      !sessionId || Buffer.byteLength(sessionId, "utf8") > 64 || encode(sessionId) !== parts[3] ||
      !/^[\w-]{43}$/.test(parts[4] ?? "") ||
      Buffer.from(parts[4], "base64url").toString("base64url") !== parts[4] ||
      runtimeRoot(apiUrl) !== apiUrl
    ) {
      throw new Error("Invalid browser session.");
    }
    return { mode: "browser", apiUrl, sessionId, handle: value };
  }
  if (!value.startsWith("cfg1.")) throw new Error("Invalid session selection.");
  const selection: unknown = JSON.parse(decode(value.slice(5)));
  if (!selection || typeof selection !== "object" || Array.isArray(selection))
    throw new Error("Invalid session selection.");
  const s = selection as Record<string, unknown>;
  const keys = Object.keys(s).sort().join(",");
  if (s.mode === "default" && keys === "mode") return { mode: "default" };
  if (s.mode === "demo" && (keys === "mode" || keys === "apiUrl,mode")) {
    if (
      s.apiUrl !== undefined &&
      (typeof s.apiUrl !== "string" || runtimeRoot(s.apiUrl) !== s.apiUrl)
    )
      throw new Error("Invalid runtime URL.");
    return { mode: "demo", apiUrl: s.apiUrl as string | undefined };
  }
  if (
    s.mode === "live" &&
    typeof s.apiUrl === "string" &&
    runtimeRoot(s.apiUrl) === s.apiUrl
  ) {
    if (s.auth === "none" && keys === "apiUrl,auth,mode")
      return { mode: "live", apiUrl: s.apiUrl, auth: "none" };
    if (
      s.auth === "static" &&
      keys === "accessToken,apiUrl,auth,mode" &&
      typeof s.accessToken === "string" &&
      s.accessToken.length > 0 &&
      s.accessToken.length <= 2048 &&
      !/[\s\x00-\x1f\x7f]/u.test(s.accessToken)
    ) {
      return {
        mode: "live",
        apiUrl: s.apiUrl,
        auth: "static",
        accessToken: s.accessToken,
      };
    }
  }
  throw new Error("Invalid session selection.");
}

export function selectionCookie(
  selection: Exclude<SessionSelection, { mode: "browser" }>,
): string {
  const value = `cfg1.${encode(JSON.stringify(selection))}`;
  parseSelection(value);
  return value;
}

export const sessionCookieOptions = () => ({
  httpOnly: true,
  secure: process.env.NODE_ENV === "production",
  sameSite: "lax" as const,
  path: "/",
  maxAge: SESSION_MAX_AGE,
});
