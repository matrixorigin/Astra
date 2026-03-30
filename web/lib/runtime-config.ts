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

  if (cookieApiUrl && cookieAccessToken) {
    return {
      mode: 'live',
      source: 'cookie',
      demoMode: false,
      apiUrl: cookieApiUrl,
      accessToken: cookieAccessToken,
      refreshToken: cookieRefreshToken,
      hasAccessToken: true,
      hasRefreshToken: Boolean(cookieRefreshToken),
      maskedAccessToken: maskToken(cookieAccessToken),
      message: 'Using saved runtime API configuration from frontend settings.',
    };
  }

  const envDemo = process.env.MO_AGENT_WEB_DEMO === 'true';
  if (envDemo) {
    return {
      mode: 'demo',
      source: 'env',
      demoMode: true,
      apiUrl: process.env.MO_AGENT_API_URL,
      accessToken: process.env.MO_AGENT_ACCESS_TOKEN,
      refreshToken: undefined,
      hasAccessToken: Boolean(process.env.MO_AGENT_ACCESS_TOKEN),
      hasRefreshToken: false,
      maskedAccessToken: maskToken(process.env.MO_AGENT_ACCESS_TOKEN),
      message: 'Demo mode is enabled from environment variables.',
    };
  }

  if (process.env.MO_AGENT_API_URL && process.env.MO_AGENT_ACCESS_TOKEN) {
    return {
      mode: 'live',
      source: 'env',
      demoMode: false,
      apiUrl: process.env.MO_AGENT_API_URL,
      accessToken: process.env.MO_AGENT_ACCESS_TOKEN,
      refreshToken: undefined,
      hasAccessToken: true,
      hasRefreshToken: false,
      maskedAccessToken: maskToken(process.env.MO_AGENT_ACCESS_TOKEN),
      message: 'Using runtime API configuration from environment variables.',
    };
  }

  return {
    mode: 'unconfigured',
    source: 'none',
    demoMode: false,
    hasAccessToken: false,
    hasRefreshToken: false,
    message: getWebConfigurationMessage(),
  };
}
