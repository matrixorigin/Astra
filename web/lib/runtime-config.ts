import { cache } from "react";
import {
  selectedSession,
  resolveBrowserSession,
} from "@/lib/auth/browser-session";
import { runtimeRoot } from "@/lib/auth/session-cookie";
import { RuntimeClientError } from "@/lib/runtime-client/errors";

/** Stable browser instance identity used to isolate Work read attachments. */
export const WEB_CLIENT_ID_COOKIE = "astra_web_client_id";
export const WEB_CLIENT_ID_HEADER = "x-astra-web-client-id";

export type WebDataMode = "live" | "demo" | "unconfigured";

export type RuntimeConfig = {
  mode: WebDataMode;
  source: "cookie" | "env" | "none";
  apiUrl?: string;
  accessToken?: string;
  refreshToken?: string;
  demoMode: boolean;
  hasAccessToken: boolean;
  hasRefreshToken: boolean;
  maskedAccessToken?: string;
  message: string;
};

export function getWebConfigurationMessage(): string {
  return "Use the Settings page to configure the runtime API URL and login token, or enable demo mode.";
}

export function maskToken(token?: string): string | undefined {
  if (!token) {
    return undefined;
  }

  if (token.length <= 10) {
    return "••••••";
  }

  return `${token.slice(0, 4)}••••${token.slice(-4)}`;
}

export const DEFAULT_API_URL = "http://localhost:17001";

// React deduplicates within a Server Component render only, never across users
// or Web replicas. Route handlers may construct more than one resolved client.
export const getRuntimeConfig = cache(async (): Promise<RuntimeConfig> => {
  const selection = await selectedSession();
  const configured = selection && selection.mode !== "default";
  const source = configured
    ? "cookie"
    : process.env.ASTRA_API_URL
      ? "env"
      : "none";
  const apiUrl =
    configured && "apiUrl" in selection && selection.apiUrl
      ? selection.apiUrl
      : runtimeRoot(process.env.ASTRA_API_URL ?? DEFAULT_API_URL);
  const demoMode = configured
    ? selection.mode === "demo"
    : process.env.ASTRA_WEB_DEMO === "true";
  let accessToken: string | undefined;
  let refreshToken: string | undefined;
  if (selection?.mode === "browser") {
    refreshToken = selection.handle;
    try {
      accessToken = await resolveBrowserSession(selection);
    } catch (error) {
      // A revoked handle stays bound to its account/runtime; no environment
      // fallback. Keep it available for idempotent logout and a fresh login.
      if (!(error instanceof RuntimeClientError && error.status === 401))
        throw error;
    }
  } else if (selection?.mode === "live") {
    if (selection.auth === "static") accessToken = selection.accessToken;
  }
  return {
    mode: demoMode ? "demo" : "live",
    source,
    apiUrl,
    accessToken,
    refreshToken,
    demoMode,
    hasAccessToken: Boolean(accessToken),
    hasRefreshToken: Boolean(refreshToken && accessToken),
    maskedAccessToken: maskToken(accessToken),
    message: demoMode
      ? "Demo mode is enabled."
      : accessToken
        ? `Connected to ${apiUrl} with authentication.`
        : `Connected to ${apiUrl} without authentication. Login for full access.`,
  };
});
