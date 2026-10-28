import { getRuntimeConfig } from "@/lib/runtime-config";
import { WebRuntimeClient, RuntimeClientError } from "@/lib/runtime-client";
import { isAuthUser, type AuthUser } from "@/lib/auth/runtime-auth-client";

export async function currentUserContext(): Promise<{
  user: AuthUser;
  client: WebRuntimeClient;
} | null> {
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
    return { user, client };
  } catch (error) {
    if (error instanceof RuntimeClientError && error.status === 401) return null;
    throw error;
  }
}
