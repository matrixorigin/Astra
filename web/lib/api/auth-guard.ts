import { NextResponse } from "next/server";
import { authErrorResponse } from "@/lib/auth/http-error";
import { currentUserContext } from "@/lib/auth/current-user";

export async function requireRuntimeUser() {
  try {
    const context = await currentUserContext();
    if (!context) {
      return {
        user: null,
        client: null,
        response: NextResponse.json(
          { error: "AUTH_REQUIRED" },
          { status: 401 },
        ),
      };
    }

    return {
      user: context.user,
      client: context.client,
      response: null,
    };
  } catch (error) {
    return { user: null, client: null, response: authErrorResponse(error) };
  }
}
