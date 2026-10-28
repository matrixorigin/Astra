import type { APIRequestContext } from "@playwright/test";

/** Enroll through the public API using the browser context's shared cookie jar. */
export async function configureRuntimeSession(
  request: APIRequestContext,
  webUrl: string,
  apiUrl: string,
  accessToken: string,
): Promise<void> {
  const response = await request.post(new URL("/api/runtime-config", webUrl).href, {
    data: { apiUrl, accessToken, demoMode: false },
    maxRedirects: 0,
  });
  try {
    if (!response.ok()) {
      // Response bodies/headers can contain credentials; report only the status.
      throw new Error(`Web runtime enrollment failed (HTTP ${response.status()}).`);
    }
  } finally {
    await response.dispose();
  }
}
