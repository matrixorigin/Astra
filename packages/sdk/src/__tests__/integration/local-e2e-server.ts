/**
 * Tiny HTTP server that mimics a subset of Astra HTTP + JSON wire shapes
 * for real-fetch integration tests (no production dependency).
 */
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { text as streamText } from 'node:stream/consumers';

import { PATH_AUTH_LOGIN, PATH_AUTH_ME, PATH_AUTH_REFRESH, PATH_SESSIONS } from '../../paths';

export type LocalE2eServer = {
  baseUrl: string;
  pathPrefix: string;
  close: () => Promise<void>;
  testUsername: string;
  testPassword: string;
  staticAccessToken: string;
};

function sendJson(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(body));
}

function readBody(req: IncomingMessage): Promise<string> {
  return streamText(req);
}

/**
 * @param pathPrefix - `''` or `'/api'`. API routes live under this prefix. `/health` is always on the server root (no prefix).
 */
export async function startLocalE2eServer(pathPrefix = ''): Promise<LocalE2eServer> {
  const pfx = (pathPrefix || '').replace(/\/$/, '');
  const testUsername = 'e2e_user';
  const testPassword = 'e2e_pass';
  const staticAccessToken = 'harness-access-token';
  const staticRefreshToken = 'harness-refresh-token';

  function apiPathname(reqPath: string): string {
    const u = new URL(reqPath, 'http://e2e/');
    const pathOnly = u.pathname;
    if (!pfx) {
      return pathOnly;
    }
    if (pathOnly === pfx) {
      return '/';
    }
    if (pathOnly.startsWith(`${pfx}/`)) {
      return pathOnly.slice(pfx.length) || '/';
    }
    return pathOnly;
  }

  const server: Server = createServer((req, res) => {
    void (async () => {
      try {
        if (!req.url) {
          res.writeHead(400);
          res.end();
          return;
        }
        const fullPath = new URL(req.url, 'http://e2e/').pathname;
        if (fullPath === '/health' && (req.method === 'GET' || req.method === 'HEAD')) {
          res.writeHead(200, { 'Content-Type': 'text/plain' });
          res.end('ok');
          return;
        }

        const pathname = apiPathname(req.url);
        const method = req.method ?? 'GET';

        if (method === 'POST' && pathname === PATH_AUTH_LOGIN) {
          const body = await readBody(req);
          const j = JSON.parse(body) as { username?: string; password?: string };
          if (j.username === testUsername && j.password === testPassword) {
            sendJson(res, 200, {
              access_token: staticAccessToken,
              refresh_token: staticRefreshToken,
              token_type: 'Bearer',
              expires_in: 3600,
            });
            return;
          }
          sendJson(res, 401, { detail: 'invalid credentials' });
          return;
        }

        if (method === 'POST' && pathname === PATH_AUTH_REFRESH) {
          const body = await readBody(req);
          const j = JSON.parse(body) as { refresh_token?: string };
          if (j.refresh_token === staticRefreshToken) {
            sendJson(res, 200, {
              access_token: `${staticAccessToken}-refreshed`,
              refresh_token: `${staticRefreshToken}-2`,
              token_type: 'Bearer',
              expires_in: 3600,
            });
            return;
          }
          sendJson(res, 401, { detail: 'invalid refresh' });
          return;
        }

        const bearerOk = (h: string | undefined) =>
          h === `Bearer ${staticAccessToken}` || h === `Bearer ${staticAccessToken}-refreshed`;

        if (method === 'GET' && pathname === PATH_AUTH_ME) {
          if (!bearerOk(req.headers.authorization)) {
            sendJson(res, 401, { detail: 'unauthorized' });
            return;
          }
          sendJson(res, 200, {
            user_id: 'harness-uid-1',
            username: testUsername,
            email: 'e2e_user@local.test',
            display_name: 'E2E',
          });
          return;
        }

        if (method === 'GET' && pathname === PATH_SESSIONS) {
          if (!bearerOk(req.headers.authorization)) {
            sendJson(res, 401, { detail: 'unauthorized' });
            return;
          }
          sendJson(res, 200, {
            sessions: [],
            total: 0,
            limit: 50,
            offset: 0,
          });
          return;
        }

        const streamRun = /^\/chat\/runs\/([^/]+)\/stream$/.exec(pathname);
        if (method === 'GET' && streamRun) {
          if (!bearerOk(req.headers.authorization)) {
            sendJson(res, 401, { detail: 'unauthorized' });
            return;
          }
          const u = new URL(req.url, 'http://e2e/');
          if (u.searchParams.get('last_index') === null) {
            res.writeHead(400);
            res.end();
            return;
          }
          res.writeHead(200, { 'Content-Type': 'text/event-stream' });
          res.end('data: {"type":"text_delta","content":"e2e"}\n\ndata: {"type":"turn_complete"}\n\n');
          return;
        }

        const runIdMatch = /^\/chat\/runs\/([^/]+)$/.exec(pathname);
        if (method === 'GET' && runIdMatch) {
          if (!bearerOk(req.headers.authorization)) {
            sendJson(res, 401, { detail: 'unauthorized' });
            return;
          }
          const runId = runIdMatch[1]!;
          sendJson(res, 200, {
            run_id: runId,
            session_id: 'sess-1',
            status: 'completed',
            events_count: 2,
            waiting_for: null,
          });
          return;
        }

        res.writeHead(404);
        res.end();
      } catch {
        res.writeHead(500);
        res.end();
      }
    })();
  });

  return new Promise((resolve, reject) => {
    server.listen(0, '127.0.0.1', () => {
      const addr = server.address();
      if (addr == null || typeof addr === 'string') {
        reject(new Error('no address'));
        return;
      }
      const port = addr.port;
      const baseUrl = `http://127.0.0.1:${port}`;
      resolve({
        baseUrl,
        pathPrefix: pfx,
        testUsername,
        testPassword,
        staticAccessToken,
        close: () =>
          new Promise((rslv, rj) => {
            server.close((e) => (e ? rj(e) : rslv()));
          }),
      });
    });
    server.on('error', reject);
  });
}
