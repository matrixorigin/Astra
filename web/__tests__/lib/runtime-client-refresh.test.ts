// @vitest-environment node
vi.mock("next/headers", () => ({ cookies: vi.fn() }));
import { cookies } from "next/headers";
import {
  WebRuntimeClient,
  requireRuntimeClient,
} from "@/lib/runtime-client/server";
import { getRuntimeConfig } from "@/lib/runtime-config";
import { SESSION_COOKIE, selectionCookie } from "@/lib/auth/session-cookie";
import { GET as sidebar } from "@/app/api/sidebar/route";
import { POST as workTurn } from "@/app/api/works/[workId]/branches/[branchId]/turns/route";
import { POST as refreshRoute } from "@/app/api/runtime-auth/refresh/route";
import { POST as logoutRoute } from "@/app/api/runtime-auth/logout/route";
import { GET as meRoute } from "@/app/api/runtime-auth/me/route";
import {
  GET as configRoute,
  POST as configure,
  DELETE as clearConfiguration,
} from "@/app/api/runtime-config/route";
import { requireRuntimeAuth, requireRuntimeUser } from "@/lib/api/auth-guard";
import { NextRequest } from "next/server";
import {
  GET as projects,
  POST as createProject,
} from "@/app/api/projects/route";
import { registerAction } from "@/lib/auth/actions";
import { RuntimeClientError } from "@/lib/runtime-client/errors";
import { POST as stopRun } from "@/app/api/chats/[chatId]/stop/route";
import { GET as capabilities } from "@/app/api/runtime/capabilities/route";

const root = "https://runtime.example/";
const handle = `bs1.${Buffer.from(root).toString("base64url")}.${Buffer.alloc(32, 1).toString("base64url")}`;
let jar: Map<string, string>;
let set: ReturnType<typeof vi.fn>;
beforeEach(() => {
  jar = new Map([[SESSION_COOKIE, handle]]);
  set = vi.fn((key: string, value: string) => jar.set(key, value));
  vi.mocked(cookies).mockResolvedValue({
    get: (name: string) =>
      jar.has(name) ? { value: jar.get(name) } : undefined,
    set,
  } as unknown as Awaited<ReturnType<typeof cookies>>);
});
afterEach(() => {
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
});

function runtimeFetch() {
  return vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = new URL(String(input)).pathname;
    if (path === "/auth/refresh") {
      expect(JSON.parse(String(init?.body)).refresh_token).toBe(handle);
      return Response.json({ access_token: "current", refresh_token: handle });
    }
    if (path === "/auth/logout")
      return Response.json({ message: "Logged out" });
    return new Headers(init?.headers).get("authorization") === "Bearer current"
      ? Response.json({
          user_id: "alice",
          username: "Alice",
          email: "alice@example.test",
          display_name: null,
        })
      : new Response("expired", { status: 401 });
  });
}

it("resolves many independent clients without touching read-only rendering cookies", async () => {
  set.mockImplementation(() => {
    throw new Error(
      "Cookies can only be modified in a Server Action or Route Handler.",
    );
  });
  vi.stubGlobal("fetch", runtimeFetch());
  const results = await Promise.all(
    Array.from({ length: 8 }, async () => {
      const client = await requireRuntimeClient({ auth: "required" });
      return client.sdk.getMe();
    }),
  );
  expect(results.every((user) => user.user_id === "alice")).toBe(true);
  expect(set).not.toHaveBeenCalled();
  const config = await getRuntimeConfig();
  const expiredClient = new WebRuntimeClient({
    ...config,
    accessToken: "expired",
  });
  expect((await expiredClient.sdk.getMe()).user_id).toBe("alice");
  expect(set).not.toHaveBeenCalled();
});

it("uses the handle runtime even when old settings and env select a different server", async () => {
  jar.set("astra_api_url", "https://unrelated.example");
  jar.set("astra_access_token", "legacy");
  vi.stubEnv("ASTRA_API_URL", "https://unrelated.example");
  vi.stubEnv("ASTRA_ACCESS_TOKEN", "another-account");
  const fetch = runtimeFetch();
  vi.stubGlobal("fetch", fetch);
  const client = await requireRuntimeClient({ auth: "required" });
  await client.sdk.getMe();
  expect(
    fetch.mock.calls.every(
      ([url]) => new URL(String(url)).origin === new URL(root).origin,
    ),
  ).toBe(true);
});

it("uses one owner for me, refresh, gates and config and exposes no credential", async () => {
  vi.stubGlobal("fetch", runtimeFetch());
  expect((await meRoute()).status).toBe(200);
  expect((await refreshRoute()).status).toBe(200);
  expect(await requireRuntimeAuth()).toBeNull();
  expect((await requireRuntimeUser()).user?.user_id).toBe("alice");
  const response = await configRoute();
  const text = await response.text();
  expect(text).toContain('"hasAccessToken":true');
  expect(text).not.toContain(handle);
  expect(text).not.toContain("current");
  expect(response.headers.has("set-cookie")).toBe(false);
  expect(set).not.toHaveBeenCalled();
});

it("does not erase a new login when an old handle logout response arrives late", async () => {
  let finish!: () => void;
  const pending = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const started = vi.fn();
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => {
      started();
      await pending;
      return Response.json({});
    }),
  );
  const response = logoutRoute();
  await vi.waitFor(() => expect(started).toHaveBeenCalledOnce());
  const newHandle = handle.replace(/.$/, handle.endsWith("A") ? "Q" : "A");
  jar.set(SESSION_COOKIE, newHandle);
  finish();
  expect((await response).status).toBe(200);
  expect(set).not.toHaveBeenCalled();
  expect(jar.get(SESSION_COOKIE)).toBe(newHandle);
});

it.each([401, 503])(
  "keeps a failed owner bound and never falls back to env (%s)",
  async (status) => {
    vi.stubEnv("ASTRA_ACCESS_TOKEN", "other-account");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({ detail: "session unavailable" }, { status }),
      ),
    );
    expect((await meRoute()).status).toBe(status);
    expect((await requireRuntimeAuth())?.status).toBe(status);
    expect((await requireRuntimeUser()).response?.status).toBe(status);
    expect(set).not.toHaveBeenCalled();
  },
);

it("reports failed logout and keeps the proof when revocation outcome is unknown", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => {
      throw new Error("connection reset");
    }),
  );
  expect((await logoutRoute()).status).toBe(503);
  expect(set).not.toHaveBeenCalled();
});

it("validates replacement before revoking and binds static credentials to their selected runtime", async () => {
  const fetch = runtimeFetch();
  vi.stubGlobal("fetch", fetch);
  const send = (body: unknown) =>
    configure(
      new NextRequest("http://web.test/api/runtime-config", {
        method: "POST",
        body: JSON.stringify(body),
        headers: { "content-type": "application/json" },
      }),
    );
  expect(
    (await send({ apiUrl: "file:///tmp", accessToken: "static" })).status,
  ).toBe(400);
  expect(fetch).not.toHaveBeenCalled();
  const response = await send({
    apiUrl: "https://other.example",
    accessToken: "static",
  });
  expect(response.status).toBe(200);
  expect(new URL(String(fetch.mock.calls[0][0])).origin).toBe(
    new URL(root).origin,
  );
  jar.set(SESSION_COOKIE, response.cookies.get(SESSION_COOKIE)!.value);
  const config = await getRuntimeConfig();
  expect(config.accessToken).toBe("static");
  expect(config.apiUrl).toBe("https://other.example/");
  expect(config.refreshToken).toBeUndefined();
});

it("static logout suppresses env fallback, while reset restores only the deployment URL", async () => {
  vi.stubEnv("ASTRA_ACCESS_TOKEN", "env-token");
  vi.stubEnv("ASTRA_API_URL", "https://env.example");
  jar.set(
    SESSION_COOKIE,
    selectionCookie({
      mode: "live",
      apiUrl: root,
      auth: "static",
      accessToken: "static",
    }),
  );
  expect((await logoutRoute()).status).toBe(200);
  expect((await getRuntimeConfig()).accessToken).toBeUndefined();
  const reset = await clearConfiguration();
  jar.set(SESSION_COOKIE, reset.cookies.get(SESSION_COOKIE)!.value);
  const config = await getRuntimeConfig();
  expect(config.accessToken).toBeUndefined();
  expect(config.apiUrl).toBe("https://env.example/");
});

it.each(["cfg1.invalid", ""])(
  "does not use a different account for invalid selector %s",
  async (value) => {
    vi.stubEnv("ASTRA_ACCESS_TOKEN", "env-token");
    jar.set(SESSION_COOKIE, value);
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    expect((await requireRuntimeAuth())?.status).toBe(401);
    expect(fetch).not.toHaveBeenCalled();
  },
);

it("resolves sidebar with the browser owner; demo configuration grants no workspace identity", async () => {
  const auth = runtimeFetch();
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const path = new URL(String(input)).pathname;
    if (path === "/sessions")
      return Response.json({ sessions: [], next_cursor: null });
    if (path === "/v1/runs" || path === "/runs")
      return Response.json({ runs: [], next_cursor: null });
    return auth(input, init);
  });
  vi.stubGlobal("fetch", fetch);
  const live = await sidebar();
  expect(live.status).toBe(200);
  expect((await live.json()).user.id).toBe("alice");
  expect(
    fetch.mock.calls.some(
      ([url]) => new URL(String(url)).pathname === "/sessions",
    ),
  ).toBe(true);
  expect(set).not.toHaveBeenCalled();
  fetch.mockClear();
  jar.set(SESSION_COOKIE, selectionCookie({ mode: "demo" }));
  const demo = await sidebar();
  expect(demo.status).toBe(401);
  expect((await requireRuntimeUser()).response?.status).toBe(401);
  expect((await logoutRoute()).status).toBe(200);
  expect((await getRuntimeConfig()).demoMode).toBe(false);
  expect(fetch).not.toHaveBeenCalled();
});

it("retries Work stream admission after a late 401 without changing its request or cookie", async () => {
  let resolutions = 0;
  const admissions: Array<{ body: string; bearer: string | null }> = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input)).pathname;
      if (path === "/auth/refresh") {
        resolutions += 1;
        expect(JSON.parse(String(init?.body)).refresh_token).toBe(handle);
        return Response.json({
          access_token: resolutions === 1 ? "old" : "current",
          refresh_token: handle,
        });
      }
      expect(path).toBe("/v1/works/work-1/branches/branch-1/turns");
      admissions.push({
        body: String(init?.body),
        bearer: new Headers(init?.headers).get("authorization"),
      });
      return admissions.length === 1
        ? new Response("expired", { status: 401 })
        : new Response('data: {"type":"work_turn_started"}\n\n', {
            headers: { "content-type": "text/event-stream" },
          });
    }),
  );
  const input = {
    request_id: "turn-1",
    attachment_id: "attachment-1",
    message: "Continue",
  };
  const response = await workTurn(
    new NextRequest(
      "http://web.test/api/works/work-1/branches/branch-1/turns",
      {
        method: "POST",
        body: JSON.stringify(input),
      },
    ),
    { params: Promise.resolve({ workId: "work-1", branchId: "branch-1" }) },
  );
  expect(response.status).toBe(200);
  expect(await response.text()).toContain("work_turn_started");
  expect(admissions).toEqual([
    { body: JSON.stringify(input), bearer: "Bearer old" },
    { body: JSON.stringify(input), bearer: "Bearer current" },
  ]);
  expect(resolutions).toBe(2);
  expect(response.headers.has("set-cookie")).toBe(false);
  expect(set).not.toHaveBeenCalled();
});

it.each([undefined, selectionCookie({ mode: "default" })])(
  "never exposes a deployment bearer to an anonymous/default browser (%s)",
  async (selector) => {
    vi.stubEnv("ASTRA_ACCESS_TOKEN", "deployment-account-secret");
    vi.stubEnv("ASTRA_API_URL", "https://env.example/");
    if (selector === undefined) jar.delete(SESSION_COOKIE);
    else jar.set(SESSION_COOKIE, selector);
    const fetch = vi.fn();
    vi.stubGlobal("fetch", fetch);
    expect((await meRoute()).status).toBe(401);
    expect((await requireRuntimeAuth())?.status).toBe(401);
    expect((await requireRuntimeUser()).response?.status).toBe(401);
    const config = await getRuntimeConfig();
    expect(config.hasAccessToken).toBe(false);
    expect(config.accessToken).toBeUndefined();
    expect(fetch).not.toHaveBeenCalled();
  },
);

it("reports an already-created account when browser enrollment fails after registration", async () => {
  jar.clear();
  const calls: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const path = new URL(String(input)).pathname;
      calls.push(path);
      if (path === "/auth/register")
        return Response.json({
          user_id: "alice",
          username: "Alice",
          email: "alice@example.test",
          display_name: null,
          access_token: "access",
          refresh_token: "refresh",
        });
      return Response.json({ detail: "unavailable" }, { status: 503 });
    }),
  );
  const form = new FormData();
  form.set("username", "Alice");
  form.set("email", "alice@example.test");
  form.set("password", "test-password");
  const result = await registerAction({ ok: false }, form);
  expect(result.ok).toBe(false);
  expect(result.error).toContain("Account created");
  expect(result.error).toContain("Sign in to continue");
  expect(calls).toEqual([
    "/auth/register",
    "/auth/browser-sessions",
    "/auth/refresh",
  ]);
  expect(set).not.toHaveBeenCalled();
});

it.each([
  {},
  { user_id: "", username: "A", email: "a@example.test", display_name: null },
  { user_id: 1, username: "A", email: "a@example.test", display_name: null },
])(
  "rejects malformed current-user JSON before any user-scoped route: %j",
  async (payload) => {
    const auth = runtimeFetch();
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        return new URL(String(input)).pathname === "/auth/me"
          ? Response.json(payload)
          : auth(input, init);
      }),
    );
    expect((await meRoute()).status).toBe(502);
    const guard = await requireRuntimeUser();
    expect(guard.user).toBeNull();
    expect(guard.response?.status).toBe(502);
    expect(
      (await projects(new NextRequest("http://web.test/api/projects"))).status,
    ).toBe(502);
    expect(
      (
        await createProject(
          new NextRequest("http://web.test/api/projects", {
            method: "POST",
            body: JSON.stringify({ name: "must not be created" }),
          }),
        )
      ).status,
    ).toBe(502);
    expect(set).not.toHaveBeenCalled();
  },
);

it("rejects a non-JSON successful current-user response before user mutations", async () => {
  const auth = runtimeFetch();
  const fetch = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    new URL(String(input)).pathname === "/auth/me"
      ? new Response("<html>upstream error</html>", { status: 200 })
      : auth(input, init),
  );
  vi.stubGlobal("fetch", fetch);
  expect((await meRoute()).status).toBe(502);
  const createRequest = new NextRequest("http://web.test/api/projects", {
    method: "POST", body: JSON.stringify({ name: "must not be created" }),
  });
  const stopRequest = new Request("http://web.test/api/chats/example/stop", {
    method: "POST", body: JSON.stringify({ sessionId: "s", runId: "r", assistantMessageId: null }),
  });
  expect((await createProject(createRequest)).status).toBe(502);
  expect((await stopRun(stopRequest, { params: Promise.resolve({ chatId: "example" }) })).status).toBe(502);
  expect(createRequest.bodyUsed).toBe(false);
  expect(stopRequest.bodyUsed).toBe(false);
  expect(fetch.mock.calls.every(([url]) => ["/auth/me", "/auth/refresh"].includes(new URL(String(url)).pathname))).toBe(true);
  expect(set).not.toHaveBeenCalled();
});

it.each([[200, 502], [302, 502], [401, 401], [403, 403], [409, 409], [429, 429], [503, 503], [600, 502], [NaN, 502]])(
  "normalizes RuntimeClientError status %s to %s", (status, expected) => {
    expect(new RuntimeClientError({ operation: "test", path: "/", detail: "failure", status }).status).toBe(expected);
  },
);

it.each([200, 201])("rejects malformed capability responses with upstream status %s", async (status) => {
  const auth = runtimeFetch();
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    new URL(String(input)).pathname === "/auth/refresh"
      ? auth(input, init) : new Response("<html>error</html>", { status }),
  ));
  expect((await capabilities()).status).toBe(502);
});

it("preserves missing error status and successful empty responses", async () => {
  expect(new RuntimeClientError({ operation: "test", path: "/", detail: "failure" }).status).toBeUndefined();
  const auth = runtimeFetch();
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    new URL(String(input)).pathname === "/auth/refresh" ? auth(input, init) : new Response(null, { status: 204 }),
  ));
  const client = await requireRuntimeClient({ auth: "required" });
  expect(await client.get("/empty")).toBeUndefined();
});
