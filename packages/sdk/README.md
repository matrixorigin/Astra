# @astra/sdk

TypeScript SDK for the Astra agent runtime: JWT auth, sessions, runs, **run list**, **multi-agent delegation**, **session lifecycle** (update / close / resume / cancel / activity), **reflect** and **decision-trace**, **events** (session timeline and causal chains), **edge connection status**, memory, **§5.5** edge callbacks, task leases, **SSE** (`POST /chat/stream`), and **WebSocket** (`/chat/ws`).

Paths match the Rust server and [`astra-thin-client`](../../rust/crates/astra-thin-client/src/paths.rs) (no `/api` prefix by default). Use `pathPrefix` if your gateway mounts the API under a prefix (for example `/api` → `https://host/api/auth/login`).

**Distribution:** This package is versioned in the Astra monorepo and consumed via `file:../packages/sdk` (see `web/package.json`). Publishing to the public npm registry is optional; set `repository` / `publishConfig` when you are ready to release.

## Installation

```bash
npm install @astra/sdk
```

(From a checkout, depend on `file:../packages/sdk` or your pack tarball.)

## Quick start

### REST client (direct to `astra-server`)

```typescript
import { AstraClient } from '@astra/sdk';

const client = new AstraClient({ baseUrl: 'http://localhost:8080' });

const auth = await client.login('alice', 'password');
// auth: access_token, refresh_token, token_type, expires_in

const session = await client.createSession();
// session.sessionId, createdAt, lastActive (normalized from server snake_case)

const run = await client.createRun({
  message: 'Hello',
  sessionId: session.sessionId,
});
```

`register(username, password, { email?, displayName? })` sends `email`; if omitted, a placeholder `{username}@users.local.astra` is used so the server’s required field is satisfied.

### Runs, delegation, and session observability

```typescript
// List durable runs (GET /runs)
const { runs, total } = await client.listRuns({ limit: 20, offset: 0 });

// Sub-runs for a parent run (GET /chat/runs/{id}/delegations)
const { sub_run_ids } = await client.listDelegations(parentRunId);

// Delegate to multiple agents (POST /chat/runs/{id}/delegate) — body matches server DelegationRequest
const me = await client.getMe();
await client.delegateRun(parentRunId, {
  delegation_id: crypto.randomUUID(),
  parent_run_id: parentRunId,
  task: 'Review and summarize',
  pattern: { sequential: { agent_ids: ['agent-a', 'agent-b'], stop_on_success: false, timeout_sec: 0 } },
  user_id: me.user_id,
  depth: 0,
  context: {},
});

await client.pauseDelegations(parentRunId);
await client.resumeDelegations(parentRunId);

// Session ops + activity log
await client.updateSession(sessionId, { title: 'Renamed' });
await client.closeSession(sessionId);
const activity = await client.getSessionActivity(sessionId, { limit: 50 });

// Reflect / tool-selection evidence (GET /chat/session/.../reflect | decision-trace)
const report = await client.getSessionReflect(sessionId, { focus: 'auto', last_n: 20, question: '' });

// Event pipeline (GET /events/session/... , GET /events?... , GET /events/causal-chain/...)
const timeline = await client.getSessionEvents(sessionId, { limit: 100 });
const filtered = await client.listEvents({ sessionId, eventType: 'tool_result', limit: 50 });
const chain = await client.getCausalChain(causalChainId);

// Connected edge agents (GET /edges/status)
const { edges } = await client.getEdgesStatus();
```

Delegation requires a configured **delegation engine** on the server; otherwise the API returns **503** — the SDK forwards errors as `AstraApiError`.

### Streaming (SSE)

`streamChat` uses **`POST /chat/stream`** with a **JSON body** (same contract as the Next.js BFF example in `web/hooks/use-chat-stream.ts`).

```typescript
const stream = client.streamChat(
  { message: 'Explain quantum computing', sessionId: 'sess-1' },
  {
    onEvent(event) {
      if (event.type === 'text_delta') {
        process.stdout.write(event.content);
      }
    },
  },
);

// stream.close() to cancel
```

### WebSocket

The runtime exposes **`/chat/ws`** (not under `/api` unless you set `pathPrefix`).

```typescript
import { AstraWebSocket } from '@astra/sdk';

const ws = new AstraWebSocket({
  url: 'ws://localhost:8080/chat/ws',
  token: auth.access_token,
});

await ws.connect();
ws.on('text_delta', (event) => console.log(event.content));
ws.sendMessage('Build a REST API', { sessionId: 'sess-1' });
```

### Browser apps and the BFF pattern

Do not expose long-lived **refresh tokens** in browser-accessible JavaScript. The dashboard (`web/`) keeps tokens in **httpOnly cookies** and proxies to the runtime via **Next.js Route Handlers** (`/api/backend/[...path]` → upstream paths like `/chat/stream`). Reuse that pattern in other SPAs (any server-side proxy that attaches `Authorization: Bearer …`).

`AstraClient` with `baseUrl` pointing at the **origin** of your BFF and `pathPrefix: '/api/backend'` is one way to align with that layout, provided the proxy forwards to the same paths the SDK calls.

### Gateway prefix

```typescript
const client = new AstraClient({
  baseUrl: 'https://api.example.com',
  pathPrefix: '/v1',
});
// e.g. login → https://api.example.com/v1/auth/login
```

### React hooks

```tsx
import { useAstraChat } from '@astra/sdk/react';
import { AstraClient } from '@astra/sdk';

const client = new AstraClient({
  baseUrl: 'http://localhost:8080',
  accessToken: token,
});

function Chat() {
  const { messages, sendMessage, isStreaming, plan, usage } = useAstraChat({
    client,
    agentId: 'optional-agent',
    model: 'optional-model',
  });

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>{m.content}</div>
      ))}
      <button type="button" onClick={() => sendMessage('Hello!')}>
        Send
      </button>
    </div>
  );
}
```

## §5.5 Edge protocol (from TypeScript)

When a **local edge** runs tools, use the same routes as `astra-thin-client`:

| Method | Client API | Route |
|--------|------------|--------|
| Tool result | `postToolResult(body, { edgeExecutorId })` | `POST /tools/result` + `X-Astra-Edge-Id` |
| Approval | `postApprovalRespond(body)` | `POST /approval/respond` |
| Register edge | `registerEdge(body, { edgeTransportId })` | `POST /agents/edge` |
| Heartbeat | `postEdgeHeartbeat(body, { edgeTransportId })` | `POST /agents/edge/heartbeat` |
| Lease | `getTaskLease`, `postTaskLeaseClaim` / `Release` / `Renew` | `/tasks/{id}/lease/...` |

Constants and path helpers are exported from `@astra/sdk` (for example `PATH_CHAT_STREAM`, `joinApiPath`, `ASTRA_EDGE_ID_HEADER`).

## API reference (high level)

### `AstraClient`

| Method | Description |
|--------|-------------|
| `register(username, password, options?)` | Register; optional `email`, `displayName` |
| `login` / `logout` / `getMe` | Auth (`logout` posts `refresh_token`) |
| `createSession` / `getSession` / `listSessions` / `deleteSession` | Sessions |
| `updateSession` / `closeSession` / `resumeSession` / `cancelSession` | `PUT` / `POST` under `/sessions/{id}` |
| `getSessionActivity` | `GET /sessions/{id}/activity` |
| `getSessionAudit` | `GET …/audit/summary` → `SessionAuditSummary` |
| `getSessionReflect` / `getSessionDecisionTrace` | `GET /chat/session/{id}/reflect` · `…/decision-trace` (optional query: `focus`, `last_n`, `question`) |
| `createRun` | `POST /chat` (non-streaming run) |
| `listRuns` | `GET /runs?limit&offset` → `RunListResponse` (`runs` normalized to `RunStatus[]`) |
| `getRunStatus` / `cancelRun` / `pauseRun` / `resumeRun` | `GET`/`DELETE`/`POST` under `/chat/runs/{id}` |
| `getRunEvents` | `GET /chat/runs/{id}/stream?last_index=` (buffered SSE parsed to `StreamEvent[]`) |
| `delegateRun` / `listDelegations` / `pauseDelegations` / `resumeDelegations` | Multi-agent: `POST …/delegate`, `GET …/delegations`, `POST …/delegations/pause` · `resume` |
| `streamChat` | `POST /chat/stream` (SSE) |
| `getSessionEvents` / `listEvents` / `getCausalChain` | `GET /events/session/{id}`, `GET /events?…`, `GET /events/causal-chain/{id}` |
| `getEdgesStatus` | `GET /edges/status` |
| `memoryStore` / `memorySearch` / … | `/memory/*` |
| `listSkills` | `GET /skills` (maps `skills[]` to `SkillInfo[]`) |
| §5.5 | `postToolResult`, `postApprovalRespond`, `registerEdge`, `postEdgeHeartbeat`, task lease methods |

### `AstraWebSocket`

| Method | Description |
|--------|-------------|
| `connect()` / `close()` | Lifecycle |
| `on` / `off` | Event subscription |
| `sendMessage` | Chat message (`session_id` in payload when given) |
| `approveToolCall` | Tool approval |

### Utilities

| Export | Description |
|--------|-------------|
| `parseSseDataEvents` | Parse full SSE text body into `StreamEvent[]` |
| `buildQueryString` | Build `?a=1&b=2` from a params object (skips undefined/null) |
| `PATH_*`, `joinApiPath`, `sessionClosePath`, `chatRunDelegatePath`, `eventsSessionPath`, … | Path constants and helpers aligned with Rust `astra-thin-client` |

## Types

Stream event types, `ChatRequest`, `RunListResponse`, `SessionAuditSummary`, `SessionUpdateBody`, `SessionActivityResponse`, `ReflectReport`, `ReflectQueryParams`, `DelegationRequestBody`, `DelegationResponse`, `EventResponse`, `EventListResponse`, `EventListFilters`, `EdgeStatusResponse`, §5.5 request bodies, and more are exported from `@astra/sdk`.

## Testing

```bash
cd packages/sdk
npm ci
npm test
npm run build
```

Jest config: `jest.config.mjs` (no `ts-node` required). Suites cover `AstraClient`, SSE/WebSocket helpers, React hooks, and **`paths`** (`buildQueryString`, `joinApiPath`, path encoders).

## License

MIT
