// @vitest-environment node

import { NextRequest } from "next/server";
import { proxy } from "@/proxy";

function request(path: string) {
  return new NextRequest(`http://web.test${path}`);
}

describe("web proxy auth boundary", () => {
  it("allows edge status probing without web auth cookies", () => {
    const response = proxy(request("/api/edges/status"));

    expect(response.status).toBe(200);
    expect(response.headers.get("x-middleware-next")).toBe("1");
  });

  it("keeps other API routes protected without web auth cookies", async () => {
    const response = proxy(request("/api/chats"));

    expect(response.status).toBe(401);
    expect(await response.json()).toEqual({
      error: "Authentication required.",
    });
  });
});
