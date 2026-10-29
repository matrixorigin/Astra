import { NextRequest, NextResponse } from "next/server";
import { DEFAULT_API_URL, getRuntimeConfig } from "@/lib/runtime-config";
import {
  SESSION_COOKIE,
  selectionCookie,
  sessionCookieOptions,
  runtimeRoot,
} from "@/lib/auth/session-cookie";
import { adoptBrowserSession } from "@/lib/auth/browser-session";
import { authErrorResponse } from "@/lib/auth/http-error";

type RuntimeConfigBody = {
  apiUrl?: string;
  accessToken?: string;
  refreshToken?: string;
  browserProof?: string;
  demoMode?: boolean;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOnlyExpectedFieldTypes(body: Record<string, unknown>): boolean {
  return (
    (body.apiUrl === undefined || typeof body.apiUrl === "string") &&
    (body.accessToken === undefined || typeof body.accessToken === "string") &&
    (body.refreshToken === undefined ||
      typeof body.refreshToken === "string") &&
    (body.browserProof === undefined || typeof body.browserProof === "string") &&
    (body.demoMode === undefined || typeof body.demoMode === "boolean")
  );
}

export async function GET() {
  try {
    const config = await getRuntimeConfig();
    return NextResponse.json({
      mode: config.mode,
      source: config.source,
      apiUrl: config.apiUrl ?? "",
      demoMode: config.demoMode,
      hasAccessToken: config.hasAccessToken,
      hasRefreshToken: config.hasRefreshToken,
      maskedAccessToken: config.maskedAccessToken ?? null,
      message: config.message,
    });
  } catch (error) {
    return authErrorResponse(error);
  }
}

export async function POST(request: NextRequest) {
  let rawBody: unknown;
  try {
    rawBody = await request.json();
  } catch {
    return NextResponse.json({ error: "invalid JSON body" }, { status: 400 });
  }

  if (!isRecord(rawBody)) {
    return NextResponse.json(
      { error: "request body must be a JSON object" },
      { status: 400 },
    );
  }

  if (!hasOnlyExpectedFieldTypes(rawBody)) {
    return NextResponse.json(
      { error: "runtime config fields have invalid types" },
      { status: 400 },
    );
  }

  const body = rawBody as RuntimeConfigBody;
  const accessToken = body.accessToken?.trim();
  const refreshToken = body.refreshToken?.trim();
  const proof = body.browserProof;
  if (refreshToken || proof !== undefined) {
    if (!refreshToken || !accessToken || body.demoMode || !proof ||
        !/^[A-Za-z0-9_-]{43}$/.test(proof) ||
        Buffer.from(proof, "base64url").toString("base64url") !== proof) {
      return NextResponse.json(
        { error: "Token-pair import requires an independent random 32-byte browserProof. Retain it across retries, or sign in." },
        { status: 400 },
      );
    }
  }
  if (body.demoMode && accessToken) {
    return NextResponse.json({ error: "Select demo mode without credentials." }, { status: 400 });
  }
  let apiUrl: string;
  let value: string;
  try {
    apiUrl = runtimeRoot(
      body.apiUrl?.trim() || process.env.ASTRA_API_URL || DEFAULT_API_URL,
    );
    // Validate the complete replacement before changing the browser selection.
    value = selectionCookie(
      body.demoMode
        ? { mode: "demo", apiUrl }
        : accessToken
          ? { mode: "live", apiUrl, auth: "static", accessToken }
          : { mode: "live", apiUrl, auth: "none" },
    );
  } catch {
    return NextResponse.json(
      { error: "Invalid or oversized runtime configuration." },
      { status: 400 },
    );
  }
  try {
    if (refreshToken && accessToken) {
      value = await adoptBrowserSession(apiUrl, { access_token: accessToken, refresh_token: refreshToken }, proof);
    }
    const response = NextResponse.json({ ok: true });
    response.cookies.set(SESSION_COOKIE, value, sessionCookieOptions());
    return response;
  } catch (error) {
    return authErrorResponse(error);
  }
}

export async function DELETE() {
  try {
    const response = NextResponse.json({ ok: true });
    response.cookies.set(
      SESSION_COOKIE,
      selectionCookie({ mode: "default" }),
      sessionCookieOptions(),
    );
    return response;
  } catch (error) {
    return authErrorResponse(error);
  }
}
