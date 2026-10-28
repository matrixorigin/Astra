"use server";

import { redirect } from "next/navigation";
import { getRuntimeConfig, DEFAULT_API_URL } from "@/lib/runtime-config";
import { WebRuntimeClient, RuntimeClientError } from "@/lib/runtime-client";
import {
  runtimeLogin,
  runtimeRegister,
  isAuthUser,
  type AuthUser,
} from "@/lib/auth/runtime-auth-client";
import {
  adoptBrowserSession,
  saveSelection,
  logoutSelection,
  replaceableSession,
  selectedSession,
  resolveBrowserSession,
} from "@/lib/auth/browser-session";

type ActionResult = {
  ok: boolean;
  error?: string;
};

async function loginApiUrl(): Promise<string> {
  const selected = await replaceableSession();
  return selected && "apiUrl" in selected && selected.apiUrl
    ? selected.apiUrl
    : (process.env.ASTRA_API_URL ?? DEFAULT_API_URL);
}

export async function loginAction(
  _prev: ActionResult,
  formData: FormData,
): Promise<ActionResult> {
  const username = formData.get("username") as string | null;
  const password = formData.get("password") as string | null;

  if (!username || !password) {
    return { ok: false, error: "Username and password are required." };
  }

  const apiUrl = await loginApiUrl();
  const result = await runtimeLogin(apiUrl, { username, password });
  if (!result.ok) {
    return { ok: false, error: result.error };
  }

  try {
    await saveSelection(await adoptBrowserSession(apiUrl, result.data));
  } catch (error) {
    return {
      ok: false,
      error:
        error instanceof Error
          ? error.message
          : "Sign-in could not be completed.",
    };
  }
  return { ok: true };
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

  const apiUrl = await loginApiUrl();

  const result = await runtimeRegister(apiUrl, {
    username,
    email,
    password,
    display_name: displayName,
  });
  if (!result.ok) {
    return { ok: false, error: result.error };
  }

  try {
    await saveSelection(await adoptBrowserSession(apiUrl, result.data));
  } catch {
    return {
      ok: false,
      error:
        "Account created, but sign-in could not be completed. Sign in to continue.",
    };
  }
  return { ok: true };
}

export async function logoutAction(): Promise<void> {
  await logoutSelection();
  redirect("/");
}

export async function refreshTokenAction(): Promise<ActionResult> {
  try {
    const selection = await selectedSession();
    if (selection?.mode !== "browser")
      return { ok: false, error: "Sign in to refresh your session." };
    await resolveBrowserSession(selection);
    return { ok: true };
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : "Refresh failed.",
    };
  }
}

export async function getCurrentUser(): Promise<AuthUser | null> {
  try {
    const config = await getRuntimeConfig();
    if (config.mode !== "live" || !config.accessToken) return null;
    const client = new WebRuntimeClient(config);
    const user = await client.get<unknown>("/auth/me", { auth: "required" });
    if (!isAuthUser(user)) {
      throw new RuntimeClientError({
        operation: "read current user",
        path: "/auth/me",
        status: 502,
        detail: "Invalid account response from runtime.",
      });
    }
    return user;
  } catch (error) {
    if (error instanceof RuntimeClientError && error.status === 401)
      return null;
    throw error;
  }
}
