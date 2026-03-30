import { cookies } from 'next/headers';

export const API_URL_COOKIE = 'mo_agent_api_url';
export const ACCESS_TOKEN_COOKIE = 'mo_agent_access_token';
export const REFRESH_TOKEN_COOKIE = 'mo_agent_refresh_token';
export const DEMO_MODE_COOKIE = 'mo_agent_demo_mode';

export type WebDataMode = 'live' | 'demo' | 'unconfigured';

export type RuntimeConfig = {
  mode: WebDataMode;
  source: 'cookie' | 'env' | 'none';
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
  return 'Use the Settings page to configure the runtime API URL and login token, or enable demo mode.';
}

export function maskToken(token?: string): string | undefined {
  if (!token) {
    return undefined;
  }

  if (token.length <= 10) {
    return '••••••';
  }

  return `${token.slice(0, 4)}••••${token.slice(-4)}`;
}

export const DEFAULT_API_URL = 'http://localhost:8000';

export async function getRuntimeConfig(): Promise<RuntimeConfig> {
  const cookieStore = await cookies();
  const cookieDemo = cookieStore.get(DEMO_MODE_COOKIE)?.value === 'true';
  const cookieApiUrl = cookieStore.get(API_URL_COOKIE)?.value;
  const cookieAccessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;
  const cookieRefreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;

  if (cookieDemo) {
    return {
      mode: 'demo',
      source: 'cookie',
      demoMode: true,
      apiUrl: cookieApiUrl,
      accessToken: cookieAccessToken,
      refreshToken: cookieRefreshToken,
      hasAccessToken: Boolean(cookieAccessToken),
      hasRefreshToken: Boolean(cookieRefreshToken),
      maskedAccessToken: maskToken(cookieAccessToken),
      message: 'Demo mode is enabled from saved frontend settings.',
    };
  }

  // Resolve API URL: cookie > env > default
  const apiUrl = cookieApiUrl ?? process.env.MO_AGENT_API_URL ?? DEFAULT_API_URL;
  const accessToken = cookieAccessToken ?? process.env.MO_AGENT_ACCESS_TOKEN;
  const refreshToken = cookieRefreshToken;

  const envDemo = process.env.MO_AGENT_WEB_DEMO === 'true';
  if (envDemo) {
    return {
      mode: 'demo',
      source: 'env',
      demoMode: true,
      apiUrl,
      accessToken,
      refreshToken,
      hasAccessToken: Boolean(accessToken),
      hasRefreshToken: Boolean(refreshToken),
      maskedAccessToken: maskToken(accessToken),
      message: 'Demo mode is enabled from environment variables.',
    };
  }

  // Live mode: API URL is always available (defaults to localhost:8000)
  const source: 'cookie' | 'env' | 'none' = cookieApiUrl
    ? 'cookie'
    : process.env.MO_AGENT_API_URL
      ? 'env'
      : 'none';

  return {
    mode: 'live',
    source,
    demoMode: false,
    apiUrl,
    accessToken,
    refreshToken,
    hasAccessToken: Boolean(accessToken),
    hasRefreshToken: Boolean(refreshToken),
    maskedAccessToken: maskToken(accessToken),
    message: accessToken
      ? `Connected to ${apiUrl} with authentication.`
      : `Connected to ${apiUrl} without authentication. Login for full access.`,
  };
}
