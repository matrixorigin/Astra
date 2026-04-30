/**
 * Tiny HTTP server that mimics a subset of Astra HTTP + JSON wire shapes
 * for real-fetch integration tests (no production dependency).
 */
import { createServer, type IncomingMessage, type Server, type ServerResponse } from 'node:http';
import { text as streamText } from 'node:stream/consumers';

import {
  PATH_AUTH_LOGIN,
  PATH_AUTH_ME,
  PATH_AUTH_REFRESH,
  PATH_CHAT_STREAM,
  PATH_MEMORY_PURGE,
  PATH_MEMORY_RETRIEVE,
  PATH_MEMORY_SEARCH,
  PATH_MEMORY_STORE,
  PATH_SESSIONS,
} from '../../paths';

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

function sendSse(res: ServerResponse, events: unknown[]): void {
  res.writeHead(200, { 'Content-Type': 'text/event-stream' });
  res.end(events.map((event) => `data: ${JSON.stringify(event)}\n\n`).join(''));
}

function readBody(req: IncomingMessage): Promise<string> {
  return streamText(req);
}

type HarnessSession = {
  session_id: string;
  user_id: string;
  agent_id: string | null;
  title: string | null;
  status: string;
  event_count: number;
  created_at: string;
  updated_at: string | null;
  ended_at: string | null;
  metadata: Record<string, unknown>;
};

type HarnessRun = {
  run_id: string;
  session_id: string;
  status: string;
  waiting_for: string | null;
  events: Array<Record<string, unknown>>;
};

type HarnessMemory = {
  id: string;
  content: string;
  memory_type?: string;
  session_id?: string;
  trust_tier?: string;
  created_at: string;
};

/**
 * @param pathPrefix - `''` or `'/api'`. API routes live under this prefix. `/health` is always on the server root (no prefix).
 */
export async function startLocalE2eServer(pathPrefix = ''): Promise<LocalE2eServer> {
  const pfx = (pathPrefix || '').replace(/\/$/, '');
  const testUsername = 'e2e_user';
  const testPassword = 'e2e_pass';
  const staticAccessToken = 'harness-access-token';
  const staticRefreshToken = 'harness-refresh-token';
  const sessions = new Map<string, HarnessSession>();
  const runs = new Map<string, HarnessRun>();
  const memories: HarnessMemory[] = [];
  const delegations = new Map<string, string[]>();
  let sessionSeq = 1;
  let runSeq = 1;
  let memorySeq = 1;
  let activitySeq = 1;

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
        const bearerOk = (h: string | undefined) =>
          h === `Bearer ${staticAccessToken}` || h === `Bearer ${staticAccessToken}-refreshed`;

        if (pathname !== PATH_AUTH_LOGIN && pathname !== PATH_AUTH_REFRESH && !bearerOk(req.headers.authorization)) {
          sendJson(res, 401, { detail: 'unauthorized' });
          return;
        }

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

        if (method === 'GET' && pathname === PATH_AUTH_ME) {
          sendJson(res, 200, {
            user_id: 'harness-uid-1',
            username: testUsername,
            email: 'e2e_user@local.test',
            display_name: 'E2E',
          });
          return;
        }

        if (method === 'POST' && pathname === PATH_SESSIONS) {
          const raw = await readBody(req);
          const body = raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
          const sessionId = `sess-${sessionSeq++}`;
          const now = new Date().toISOString();
          const session: HarnessSession = {
            session_id: sessionId,
            user_id: 'harness-uid-1',
            agent_id: typeof body.agent_id === 'string' ? body.agent_id : null,
            title: typeof body.title === 'string' ? body.title : null,
            status: 'active',
            event_count: 0,
            created_at: now,
            updated_at: now,
            ended_at: null,
            metadata:
              body.metadata && typeof body.metadata === 'object' && !Array.isArray(body.metadata)
                ? (body.metadata as Record<string, unknown>)
                : {},
          };
          sessions.set(sessionId, session);
          sendJson(res, 200, session);
          return;
        }

        if (method === 'GET' && pathname === PATH_SESSIONS) {
          sendJson(res, 200, {
            sessions: [...sessions.values()],
            total: sessions.size,
            limit: 50,
            offset: 0,
          });
          return;
        }

        const sessionRoute = /^\/sessions\/([^/]+)(?:\/([^/]+)(?:\/([^/]+))?)?$/.exec(pathname);
        if (sessionRoute) {
          const sessionId = decodeURIComponent(sessionRoute[1]!);
          const action = sessionRoute[2];
          const subAction = sessionRoute[3];
          const session = sessions.get(sessionId);
          if (!session) {
            sendJson(res, 404, { detail: 'session not found' });
            return;
          }

          if (method === 'GET' && !action) {
            sendJson(res, 200, session);
            return;
          }

          if (method === 'POST' && action === 'close') {
            session.status = 'closed';
            session.ended_at = new Date().toISOString();
            session.updated_at = session.ended_at;
            sendJson(res, 200, session);
            return;
          }

          if (method === 'GET' && action === 'activity') {
            sendJson(res, 200, {
              session_id: sessionId,
              activities: [...runs.values()]
                .filter((run) => run.session_id === sessionId)
                .flatMap((run) =>
                  run.events.map((event) => ({
                    log_id: `log-${activitySeq++}`,
                    action: event.type,
                    details: event,
                    created_at: new Date().toISOString(),
                  })),
                ),
              total: session.event_count,
            });
            return;
          }

          if (method === 'GET' && action === 'audit' && subAction === 'summary') {
            const sessionRuns = [...runs.values()].filter((run) => run.session_id === sessionId);
            sendJson(res, 200, {
              session_id: sessionId,
              status: session.status,
              turn_count: sessionRuns.length,
              tokens_in: sessionRuns.length * 10,
              tokens_out: sessionRuns.length * 20,
              tool_calls_total: sessionRuns
                .flatMap((run) => run.events)
                .filter((event) => event.type === 'tool_call_start').length,
              tool_calls_failed: 0,
              error_count: 0,
              stall_count: 0,
              checkpoint_count: 0,
              compact_count: 0,
              execution_boundary_opened_count: 0,
              execution_boundary_committed_count: 0,
              execution_boundary_aborted_count: 0,
              approval_required_count: 0,
              approval_decision_count: 0,
              approval_timeout_count: 0,
              models_used: ['harness-model'],
              duration_secs: 0,
              created_at: session.created_at,
              ended_at: session.ended_at,
            });
            return;
          }
        }

        if (method === 'POST' && pathname === PATH_CHAT_STREAM) {
          const raw = await readBody(req);
          const body = raw ? (JSON.parse(raw) as Record<string, unknown>) : {};
          const requestedSession =
            typeof body.session_id === 'string' ? sessions.get(body.session_id) : undefined;
          if (body.session_id && !requestedSession) {
            sendJson(res, 404, { detail: 'session not found' });
            return;
          }
          if (requestedSession?.status === 'closed') {
            sendJson(res, 409, { detail: 'session is closed' });
            return;
          }

          const session =
            requestedSession ??
            (() => {
              const sessionId = `sess-${sessionSeq++}`;
              const now = new Date().toISOString();
              const created: HarnessSession = {
                session_id: sessionId,
                user_id: 'harness-uid-1',
                agent_id: typeof body.agent_id === 'string' ? body.agent_id : null,
                title: null,
                status: 'active',
                event_count: 0,
                created_at: now,
                updated_at: now,
                ended_at: null,
                metadata: {},
              };
              sessions.set(sessionId, created);
              return created;
            })();

          const runId = `run-${runSeq++}`;
          const turn = [...runs.values()].filter((run) => run.session_id === session.session_id).length + 1;
          const context = body.context && typeof body.context === 'object' ? body.context : {};
          const events = [
            { type: 'session_info', session_id: session.session_id, run_id: runId },
            { type: 'run_started', run_id: runId, session_id: session.session_id },
            {
              type: 'text_delta',
              content: `turn=${turn};message=${body.message};context=${JSON.stringify(context)}`,
            },
            { type: 'usage', prompt_tokens: 10, completion_tokens: 20, cache_read_tokens: turn > 1 ? 100 : 0 },
            { type: 'turn_complete' },
          ];
          runs.set(runId, {
            run_id: runId,
            session_id: session.session_id,
            status: 'completed',
            waiting_for: null,
            events,
          });
          session.event_count += events.length;
          session.updated_at = new Date().toISOString();
          sendSse(res, events);
          return;
        }

        if (method === 'POST' && pathname === PATH_MEMORY_STORE) {
          const body = JSON.parse(await readBody(req)) as Partial<HarnessMemory>;
          const memory: HarnessMemory = {
            id: `mem-${memorySeq++}`,
            content: String(body.content ?? ''),
            memory_type: body.memory_type,
            session_id: body.session_id,
            trust_tier: body.trust_tier,
            created_at: new Date().toISOString(),
          };
          memories.push(memory);
          sendJson(res, 200, { id: memory.id });
          return;
        }

        if (method === 'POST' && (pathname === PATH_MEMORY_SEARCH || pathname === PATH_MEMORY_RETRIEVE)) {
          const body = JSON.parse(await readBody(req)) as { query?: string; top_k?: number };
          const terms = String(body.query ?? '')
            .toLowerCase()
            .split(/\s+/)
            .filter(Boolean);
          const topK = body.top_k ?? (pathname === PATH_MEMORY_RETRIEVE ? 5 : 10);
          const results = memories
            .map((memory) => {
              const text = memory.content.toLowerCase();
              const hits = terms.filter((term) => text.includes(term)).length;
              return {
                id: memory.id,
                content: memory.content,
                score: hits / Math.max(terms.length, 1),
                memory_type: memory.memory_type,
                created_at: memory.created_at,
              };
            })
            .filter((memory) => memory.score > 0)
            .sort((a, b) => b.score - a.score)
            .slice(0, topK);
          sendJson(res, 200, results);
          return;
        }

        if (method === 'POST' && pathname === PATH_MEMORY_PURGE) {
          const body = JSON.parse(await readBody(req)) as { topic?: string };
          const topic = String(body.topic ?? '').toLowerCase();
          if (!topic) {
            sendJson(res, 400, { detail: 'topic required' });
            return;
          }
          for (let i = memories.length - 1; i >= 0; i--) {
            if (memories[i].content.toLowerCase().includes(topic)) {
              memories.splice(i, 1);
            }
          }
          sendJson(res, 200, {});
          return;
        }

        const streamRun = /^\/chat\/runs\/([^/]+)\/stream$/.exec(pathname);
        if (method === 'GET' && streamRun) {
          const u = new URL(req.url, 'http://e2e/');
          if (u.searchParams.get('last_index') === null) {
            res.writeHead(400);
            res.end();
            return;
          }
          const runId = decodeURIComponent(streamRun[1]!);
          const run = runs.get(runId);
          const start = Number(u.searchParams.get('last_index') ?? '0');
          sendSse(
            res,
            run
              ? run.events.slice(start)
              : [{ type: 'text_delta', content: 'e2e' }, { type: 'turn_complete' }],
          );
          return;
        }

        const delegateMatch = /^\/chat\/runs\/([^/]+)\/delegate$/.exec(pathname);
        if (method === 'POST' && delegateMatch) {
          const runId = decodeURIComponent(delegateMatch[1]!);
          const body = JSON.parse(await readBody(req)) as { delegation_id?: string; pattern?: unknown };
          const agentIds = extractAgentIds(body.pattern);
          const childRuns = agentIds.map((agentId) => `${runId}-${agentId}-child`);
          delegations.set(runId, childRuns);
          sendJson(res, 200, {
            delegation_id: body.delegation_id ?? `delegation-${runId}`,
            status: 'completed',
            agent_results: agentIds.map((agentId) => ({
              agent_id: agentId,
              status: 'completed',
              output: `ok:${agentId}`,
              error: null,
            })),
            aggregated_output: childRuns.join(','),
            total_prompt_tokens: agentIds.length * 10,
            total_completion_tokens: agentIds.length * 20,
            total_tool_calls: agentIds.length,
          });
          return;
        }

        const delegationsMatch = /^\/chat\/runs\/([^/]+)\/delegations(?:\/(pause|resume))?$/.exec(pathname);
        if (delegationsMatch) {
          const runId = decodeURIComponent(delegationsMatch[1]!);
          const subRunIds = delegations.get(runId) ?? [];
          if (method === 'GET' && !delegationsMatch[2]) {
            sendJson(res, 200, { parent_run_id: runId, sub_run_ids: subRunIds });
            return;
          }
          if (method === 'POST' && delegationsMatch[2]) {
            sendJson(res, 200, { parent_run_id: runId, affected: subRunIds.length });
            return;
          }
        }

        const runIdMatch = /^\/chat\/runs\/([^/]+)$/.exec(pathname);
        if (method === 'GET' && runIdMatch) {
          const runId = runIdMatch[1]!;
          const run = runs.get(runId);
          sendJson(res, 200, {
            run_id: runId,
            session_id: run?.session_id ?? 'sess-1',
            status: run?.status ?? 'completed',
            events_count: run?.events.length ?? 2,
            waiting_for: run?.waiting_for ?? null,
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

function extractAgentIds(pattern: unknown): string[] {
  const out = new Set<string>();
  function visit(value: unknown, key?: string): void {
    if (Array.isArray(value)) {
      for (const item of value) visit(item, key);
      return;
    }
    if (value && typeof value === 'object') {
      for (const [childKey, childValue] of Object.entries(value)) visit(childValue, childKey);
      return;
    }
    if (
      typeof value === 'string' &&
      value.includes('agent') &&
      (key === 'agent_id' || key === 'agent_ids' || key === 'agents')
    ) {
      out.add(value);
    }
  }
  visit(pattern);
  return out.size > 0 ? [...out] : ['agent-a'];
}
