import { NextResponse } from "next/server";
import { getSidebar } from "@/lib/api/web-store";
import { getCurrentUser } from "@/lib/auth/actions";
import { authErrorResponse } from "@/lib/auth/http-error";

export async function GET() {
  try {
    const current = await getCurrentUser();
    if (!current)
      return NextResponse.json({ error: "AUTH_REQUIRED" }, { status: 401 });
    const user = {
      id: current.user_id,
      name: current.display_name ?? current.username,
      plan: "free" as const,
    };
    return NextResponse.json({ ...(await getSidebar(user.id)), user });
  } catch (error) {
    return authErrorResponse(error);
  }
}
