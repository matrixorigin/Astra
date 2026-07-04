// @vitest-environment node

import { NextRequest } from "next/server";
import { proxy } from "@/proxy";

function request(path: string) {
  return new NextRequest(`http://web.test${path}`);
}

describe("web proxy auth boundary", () => {
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
