import { NextResponse } from "next/server";
import { getSidebar } from "@/lib/api/web-store";
import { currentUserContext } from "@/lib/auth/current-user";
import { authErrorResponse } from "@/lib/auth/http-error";

export async function GET() {
  try {
    const context = await currentUserContext();
    if (!context)
      return NextResponse.json({ error: "AUTH_REQUIRED" }, { status: 401 });
    const current = context.user;
    const user = {
      id: current.user_id,
      name: current.display_name ?? current.username,
      plan: "free" as const,
    };
    return NextResponse.json({ ...(await getSidebar(user.id, context.client)), user });
  } catch (error) {
    return authErrorResponse(error);
  }
}
