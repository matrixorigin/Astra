// @vitest-environment node

import { NextRequest } from "next/server";
import { proxy } from "@/proxy";

function request(path: string) {
  return new NextRequest(`http://web.test${path}`);
}

describe("web proxy auth boundary", () => {
  it("preserves the requested path without RSC transport data or caller headers", () => {
    const req = new NextRequest("http://web.test/now?source=review&_rsc=internal", {
      headers: { "x-astra-request-path": "//attacker.example" },
    });
    req.cookies.set("astra_session", `cfg1.${Buffer.from(JSON.stringify({ mode: "live", apiUrl: "http://runtime/", auth: "static", accessToken: "test" })).toString("base64url")}`);
    const response = proxy(req);
    expect(response.headers.get("x-middleware-request-x-astra-request-path")).toBe("/now?source=review");
  });
  it("keeps edge status protected without web auth cookies", async () => {
    const response = proxy(request("/api/edges/status"));

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      error: "Authentication required.",
    });
  });

  it("keeps other API routes protected without web auth cookies", async () => {
    const response = proxy(request("/api/chats"));

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      error: "Authentication required.",
    });
  });
});

afterEach(() => vi.unstubAllEnvs());

it("never borrows deployment authentication for demo or an empty selector", () => {
  vi.stubEnv("ASTRA_WEB_DEMO", "true");
  expect(proxy(request("/projects")).status).toBe(307);
  vi.stubEnv("ASTRA_ACCESS_TOKEN", "other-account");
  const empty = request("/api/chats");
  empty.cookies.set("astra_session", "");
  expect(proxy(empty).status).toBe(401);
});

it("denies anonymous and caller-selected default configurations despite a deployment token", () => {
  vi.stubEnv("ASTRA_ACCESS_TOKEN", "deployment-secret");
  for (const cookie of [
    undefined,
    `cfg1.${Buffer.from(JSON.stringify({ mode: "default" })).toString("base64url")}`,
  ]) {
    const req = request("/api/chats");
    if (cookie) req.cookies.set("astra_session", cookie);
    expect(proxy(req).status).toBe(401);
  }
});
