// @vitest-environment node
import { createServer, type IncomingMessage } from "node:http";
import { WebRuntimeClient } from "@/lib/runtime-client/server";

function jwtForSubject(subject: string): string {
  const payload = Buffer.from(JSON.stringify({ sub: subject })).toString("base64url");
  return `e30.${payload}.`;
}

async function listen(): Promise<{ url: string; received: Promise<IncomingMessage>; close: () => Promise<void> }> {
  let resolveRequest: (request: IncomingMessage) => void;
  const received = new Promise<IncomingMessage>((resolve) => {
    resolveRequest = resolve;
  });
  const server = createServer((request, response) => {
    resolveRequest(request);
    request.resume();
    response.writeHead(204);
    response.end();
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("expected a TCP address");
  }
  return {
    url: `http://127.0.0.1:${address.port}/`,
    received,
    close: () => new Promise((resolve, reject) => server.close((error) => (error ? reject(error) : resolve()))),
  };
}

describe("WebRuntimeClient X-User-Id header encoding", () => {
  it.each(["用户-张旭", "café", "user-🚀"])(
    "sends UTF-8 bytes for subject %s through the real Web client and Headers path",
    async (subject) => {
      const server = await listen();
      try {
        const client = new WebRuntimeClient({
          mode: "live",
          apiUrl: server.url,
          accessToken: jwtForSubject(subject),
        } as never);
        await client.fetchResponse("/v1/health", { method: "GET", auth: "optional" });
        const request = await server.received;
        const header = request.headers["x-user-id"];
        expect(header).toBeDefined();
        const raw = Array.isArray(header) ? header[0] : header;
        expect(Buffer.from(raw ?? "", "latin1")).toEqual(Buffer.from(subject, "utf8"));
      } finally {
        await server.close();
      }
    },
  );
});
