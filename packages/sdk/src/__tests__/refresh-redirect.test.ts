import { createServer, type Server } from "node:http";
import { once } from "node:events";
import { AstraClient } from "../client";

async function listen(server: Server): Promise<string> {
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("missing listener");
  return `http://127.0.0.1:${address.port}`;
}

it.each([307, 308])(
  "never forwards refresh proof through an HTTP %s redirect",
  async (status) => {
    let exposed = false;
    const other = createServer((_request, response) => {
      exposed = true;
      response.end("{}");
    });
    const otherUrl = await listen(other);
    const runtime = createServer((request, response) => {
      if (request.url === "/auth/refresh")
        response.writeHead(status, { location: `${otherUrl}/stolen` });
      else response.writeHead(401);
      response.end();
    });
    const root = await listen(runtime);
    try {
      const client = new AstraClient({
        baseUrl: root,
        accessToken: "expired",
        refreshToken: "independent-browser-proof",
      });
      await expect(client.getMe()).rejects.toThrow();
      expect(exposed).toBe(false);
    } finally {
      runtime.closeAllConnections();
      other.closeAllConnections();
      await Promise.all([
        new Promise<void>((resolve) => runtime.close(() => resolve())),
        new Promise<void>((resolve) => other.close(() => resolve())),
      ]);
    }
  },
);
