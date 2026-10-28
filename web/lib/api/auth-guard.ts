import { NextResponse } from "next/server";
import { getRuntimeConfig } from "@/lib/runtime-config";
import { authErrorResponse } from "@/lib/auth/http-error";
import { getCurrentUser } from "@/lib/auth/actions";

export async function requireRuntimeAuth() {
  try {
    const config = await getRuntimeConfig();
    if (config.mode !== "live" || !config.apiUrl || !config.hasAccessToken) {
      return NextResponse.json({ error: "AUTH_REQUIRED" }, { status: 401 });
    }
    return null;
  } catch (error) {
    return authErrorResponse(error);
  }
}

export async function requireRuntimeUser() {
  try {
    const user = await getCurrentUser();
    if (!user) {
      return {
        user: null,
        response: NextResponse.json(
          { error: "AUTH_REQUIRED" },
          { status: 401 },
        ),
      };
    }

    return {
      user,
      response: null,
    };
  } catch (error) {
    return { user: null, response: authErrorResponse(error) };
  }
}
