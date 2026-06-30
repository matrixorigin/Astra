"use server";

import { cookies } from "next/headers";
import { redirect } from "next/navigation";
import {
  API_URL_COOKIE,
  ACCESS_TOKEN_COOKIE,
  REFRESH_TOKEN_COOKIE,
  DEFAULT_API_URL,
} from "@/lib/runtime-config";
import {
  runtimeExternalLogin,
  runtimeExternalProviders,
  runtimeLogin,
  runtimeLogout,
  runtimeMe,
  runtimeRefresh,
  runtimeRegister,
  type AuthTokens,
  type AuthUser,
  type ExternalAuthProvider,
} from "@/lib/auth/runtime-auth-client";

type ActionResult = {
  ok: boolean;
  error?: string;
};

type ExternalProvidersActionResult =
  | { ok: true; providers: ExternalAuthProvider[] }
  | { ok: false; error: string };

const TOKEN_MAX_AGE = 365 * 24 * 60 * 60; // 1 year for cookie persistence

function getApiUrl(cookieStore: Awaited<ReturnType<typeof cookies>>): string {
  return (
    cookieStore.get(API_URL_COOKIE)?.value ??
    process.env.ASTRA_API_URL ??
    DEFAULT_API_URL
  );
}

async function saveTokens(tokens: AuthTokens): Promise<void> {
  const cookieStore = await cookies();
  const opts = {
    httpOnly: true,
    secure: process.env.NODE_ENV === "production",
    sameSite: "lax" as const,
    path: "/",
    maxAge: TOKEN_MAX_AGE,
  };

  cookieStore.set(ACCESS_TOKEN_COOKIE, tokens.access_token, opts);
  cookieStore.set(REFRESH_TOKEN_COOKIE, tokens.refresh_token, opts);
}

export async function loginAction(
  _prev: ActionResult,
  formData: FormData,
): Promise<ActionResult> {
  const mode =
    formData.get("auth_mode") === "external" ? "external" : "internal";
  const username = formData.get("username") as string | null;
  const password = formData.get("password") as string | null;

  if (!username || !password) {
    return { ok: false, error: "Username and password are required." };
  }

  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const result =
    mode === "external"
      ? await loginWithExternalProvider(apiUrl, formData, username, password)
      : await runtimeLogin(apiUrl, { username, password });
  if (!result.ok) {
    return { ok: false, error: result.error };
  }

  await saveTokens(result.data);
  return { ok: true };
}

async function loginWithExternalProvider(
  apiUrl: string,
  formData: FormData,
  username: string,
  password: string,
) {
  const providerId = externalProviderId(formData);
  if (!providerId) {
    return {
      ok: false,
      status: 400,
      error: "External provider is required.",
    } as const;
  }

  return runtimeExternalLogin(apiUrl, {
    provider_id: providerId,
    username,
    password,
    scope_id: optionalString(formData.get("scope_id")),
  });
}

function externalProviderId(formData: FormData): string | undefined {
  const providerId = formData.get("provider_id");
  if (typeof providerId !== "string" || providerId.length === 0) {
    return undefined;
  }
  return providerId;
}

function optionalString(value: FormDataEntryValue | null): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

export async function externalProvidersAction(): Promise<ExternalProvidersActionResult> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const result = await runtimeExternalProviders(apiUrl);
  if (!result.ok) {
    return { ok: false, error: result.error };
  }
  return { ok: true, providers: result.data.providers };
}

export async function registerAction(
  _prev: ActionResult,
  formData: FormData,
): Promise<ActionResult> {
  const username = formData.get("username") as string;
  const email = formData.get("email") as string;
  const password = formData.get("password") as string;
  const displayName = (formData.get("display_name") as string) || undefined;

  if (!username || !email || !password) {
    return { ok: false, error: "Username, email, and password are required." };
  }

  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);

  const result = await runtimeRegister(apiUrl, {
    username,
    email,
    password,
    display_name: displayName,
  });
  if (!result.ok) {
    return { ok: false, error: result.error };
  }

  await saveTokens(result.data);
  return { ok: true };
}

export async function logoutAction(): Promise<void> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;

  if (apiUrl && refreshToken) {
    await runtimeLogout(apiUrl, refreshToken);
  }

  cookieStore.delete(ACCESS_TOKEN_COOKIE);
  cookieStore.delete(REFRESH_TOKEN_COOKIE);
  redirect("/");
}

export async function refreshTokenAction(): Promise<ActionResult> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const refreshToken = cookieStore.get(REFRESH_TOKEN_COOKIE)?.value;

  if (!refreshToken) {
    return { ok: false, error: "No refresh token available." };
  }

  const result = await runtimeRefresh(apiUrl, refreshToken);
  if (!result.ok) {
    return { ok: false, error: result.error };
  }

  await saveTokens(result.data);
  return { ok: true };
}

export async function getCurrentUser(): Promise<AuthUser | null> {
  const cookieStore = await cookies();
  const apiUrl = getApiUrl(cookieStore);
  const accessToken = cookieStore.get(ACCESS_TOKEN_COOKIE)?.value;

  if (!accessToken) return null;

  const result = await runtimeMe(apiUrl, accessToken);
  if (result.ok) {
    return result.data;
  }

  if (result.status !== 401) {
    return null;
  }

  const refreshResult = await refreshTokenAction();
  if (!refreshResult.ok) return null;

  const newCookieStore = await cookies();
  const newToken = newCookieStore.get(ACCESS_TOKEN_COOKIE)?.value;
  if (!newToken) return null;

  const retryResult = await runtimeMe(apiUrl, newToken);
  return retryResult.ok ? retryResult.data : null;
}
