// @vitest-environment node
import { createServer, type Server } from "node:http";
import { once } from "node:events";
import { request, type APIRequestContext } from "@playwright/test";
import { configureRuntimeSession } from "@/e2e-live/runtime-session";

let server: Server;
let browserRequest: APIRequestContext;
let webUrl: string;
let status: number;
let requests: { path: string; cookie?: string; body: string }[];

beforeEach(async () => {
  status = 200;
  requests = [];
  server = createServer(async (req, res) => {
    let body = "";
    for await (const chunk of req) body += chunk;
    requests.push({ path: req.url!, cookie: req.headers.cookie, body });
    if (req.url === "/api/runtime-config") {
      res.statusCode = status;
      if (status === 200) res.setHeader("set-cookie", "astra_session=selected; Path=/; HttpOnly");
      if (status === 307) res.setHeader("location", "/must-not-follow");
      res.end(status === 200 ? '{"ok":true}' : "private enrollment diagnostic");
    } else res.end("{}");
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Missing test listener");
  webUrl = `http://127.0.0.1:${address.port}`;
  browserRequest = await request.newContext();
});

afterEach(async () => {
  await browserRequest?.dispose();
  if (server) await new Promise<void>((resolve) => server.close(() => resolve()));
});

it("enrolls through the public API and shares the server's cookie on following requests", async () => {
  await configureRuntimeSession(browserRequest, webUrl, "https://runtime.example", "test-token");
  await browserRequest.get(`${webUrl}/api/runtime-auth/me`);
  expect(requests.map(({ path }) => path)).toEqual(["/api/runtime-config", "/api/runtime-auth/me"]);
  expect(JSON.parse(requests[0].body)).toEqual({
    apiUrl: "https://runtime.example", accessToken: "test-token", demoMode: false,
  });
  expect(requests[1].cookie).toBe("astra_session=selected");
});

it.each([400, 503, 307])("fails closed for enrollment HTTP %s without following redirects or leaking diagnostics", async (responseStatus) => {
  status = responseStatus;
  await expect(configureRuntimeSession(browserRequest, webUrl, "https://runtime.example", "test-token"))
    .rejects.toThrow(`Web runtime enrollment failed (HTTP ${status}).`);
  expect(requests).toHaveLength(1);
  expect((await browserRequest.storageState()).cookies).toEqual([]);
});
