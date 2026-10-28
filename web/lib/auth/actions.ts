"use server";

import { redirect } from "next/navigation";
import { DEFAULT_API_URL } from "@/lib/runtime-config";
import { runtimeRoot } from "@/lib/auth/session-cookie";
import { currentUserContext } from "@/lib/auth/current-user";
import {
  runtimeLogin,
  runtimeRegister,
  type AuthUser,
} from "@/lib/auth/runtime-auth-client";
import {
  adoptBrowserSession,
  saveSelection,
  logoutSelection,
  replaceableSession,
} from "@/lib/auth/browser-session";

type ActionResult = {
  ok: boolean;
  error?: string;
};

async function loginApiUrl(formData: FormData): Promise<string> {
  const requested = formData.get("runtime_url");
  if (typeof requested === "string" && requested.trim()) return runtimeRoot(requested.trim());
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

  let apiUrl: string;
  try { apiUrl = await loginApiUrl(formData); }
  catch { return { ok: false, error: "Invalid runtime URL." }; }
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

  let apiUrl: string;
  try { apiUrl = await loginApiUrl(formData); }
  catch { return { ok: false, error: "Invalid runtime URL." }; }

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

export async function getCurrentUser(): Promise<AuthUser | null> {
  return (await currentUserContext())?.user ?? null;
}
