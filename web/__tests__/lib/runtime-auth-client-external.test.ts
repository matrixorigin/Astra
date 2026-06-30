// @vitest-environment node

import {
  runtimeExternalLogin,
  runtimeExternalProviders,
} from "@/lib/auth/runtime-auth-client";

describe("runtime external auth client", () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("fetches providers from the external providers endpoint", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          providers: [
            { id: "moi", display_name: "MOI", credential_type: "password" },
          ],
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await runtimeExternalProviders("http://astra.test");

    expect(result).toEqual({
      ok: true,
      data: {
        providers: [
          { id: "moi", display_name: "MOI", credential_type: "password" },
        ],
      },
    });
    expect(fetchMock.mock.calls[0]?.[0]).toBe(
      "http://astra.test/auth/external/providers",
    );
  });

  it("posts external login without exposing provider session handles", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          access_token: "astra-access",
          refresh_token: "astra-refresh",
          provider_session_handle: "must-not-be-used",
        }),
        { status: 200, headers: { "content-type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await runtimeExternalLogin("http://astra.test", {
      provider_id: "moi",
      username: "admin",
      password: "admin",
    });

    expect(result).toEqual({
      ok: true,
      data: {
        access_token: "astra-access",
        refresh_token: "astra-refresh",
      },
    });
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit;
    expect(fetchMock.mock.calls[0]?.[0]).toBe(
      "http://astra.test/auth/external/login",
    );
    expect(JSON.parse(init.body as string)).toEqual({
      provider_id: "moi",
      username: "admin",
      password: "admin",
    });
  });
});
