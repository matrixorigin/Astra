# @astra/sdk

TypeScript SDK for the Astra agent runtime: JWT auth, sessions, runs, **run list**, **multi-agent delegation**, **session lifecycle** (update / close / resume / cancel / activity), **reflect** and **decision-trace**, **events** (session timeline and causal chains), **edge connection status**, memory, **§5.5** edge callbacks, **SSE** (`POST /chat/stream`), and **WebSocket** (`/chat/ws`).

Supported paths match the currently registered Rust runtime routes and [`astra-thin-client`](../../crates/astra-thin-client/src/paths.rs) (no `/api` prefix by default). Use `pathPrefix` if your gateway mounts the API under a prefix (for example `/api` → `https://host/api/auth/login`). Work and Run APIs use the current Server-owned lifecycle. Retired task-lease, task-plan, and agent-job routes are unavailable and are not exposed by this SDK.

**Distribution:** This package is versioned in the Astra monorepo and configured
as a public scoped npm package. Publishing remains an explicit release action;
no repository workflow publishes it automatically. The Web app consumes the
workspace copy via `file:../packages/sdk` (see `web/package.json`).

## Installation

```bash
npm install @astra/sdk
```

Before the first npm release, consume `file:../packages/sdk` from a checkout or
install a locally generated pack tarball.

## Quick start

### REST client (direct to `astra-server`)

```typescript
import { AstraClient } from '@astra/sdk';

const client = new AstraClient({ baseUrl: 'http://localhost:17001' });

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

Custom HTTP headers are merged case-insensitively. The current access token
replaces a configured Authorization header for REST and SSE requests; explicit
per-request headers retain their override precedence. SSE POST requests preserve
a custom Content-Type regardless of its casing.

### Runs, delegation, and session observability

For HTTP chat streaming, `modelSelection: "auto"` opts into the Server's
configured model-routing policy. Explicit `{ offeringId: "..." }` selection
keeps its existing behavior. Auto requires a qualified Server-catalog pair;
provider model gateways and WebSocket chat continue to use explicit Offerings.
See [model routing](../../docs/design/model-routing.md#deterministic-auto-selection-stage-3).

```typescript
// List durable runs (GET /runs)
const firstPage = await client.listRuns({ limit: 20 });
const secondPage = firstPage.nextCursor
  ? await client.listRuns({ limit: 20, cursor: firstPage.nextCursor })
  : null;

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

// Cancel execution without deleting history. A pending response is not success:
// keep showing "Stopping" and repeat this operation until executionSettled.
const cancellation = await client.cancelSession(sessionId);
if (!cancellation.executionSettled) {
  console.log('Stopping; execution is not yet confirmed idle', cancellation.runs);
}

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

Calling `SSEClient.close()` or aborting its external signal interrupts a pending
retry delay and settles `connect()` without another fetch attempt. Explicitly
calling `connect()` again can reopen a closed client if its external signal has
not been aborted.
Heartbeat timeouts abort the stalled fetch and use the same bounded retry
budget as transport failures. Each timeout emits one error whose `retryable`
flag reflects the remaining budget; user cancellation still interrupts backoff.

`streamChat` uses **`POST /chat/stream`** with a **JSON body** (the Web
dashboard's typed BFF integration lives in
`web/app/api/chats/[chatId]/stream/route.ts`).

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
  url: 'ws://localhost:17001/chat/ws',
  token: auth.access_token,
});

await ws.connect();
ws.on('text_delta', (event) => console.log(event.content));
ws.sendMessage('Build a REST API', { sessionId: 'sess-1' });
```

Concurrent calls to `connect()` share one connection attempt. It resolves after
authentication and any active run's reattachment, before success callbacks run.
`close()` rejects unfinished attempts, stops automatic reconnection, and closes
the local transport without cancelling the server run. State and event callbacks
may reconnect synchronously; callback exceptions are isolated from the transport.
Closing or reconnecting in a stream callback does not interrupt delivery of an
already accepted frame to other subscribed callbacks.

### Browser apps and the BFF pattern

Within one `AstraClient`, concurrent REST, live streams and buffered run-event requests share
one token refresh. A delayed 401 reuses the rotated token. Refresh responses
started before `setTokens`, successful login/registration, or logout cannot
restore the previous credentials or retry an old request under a new account.
`setTokens(accessToken)` clears any previous refresh token; pass both tokens
when replacing a credential pair. Pending login and logout operations cannot
overwrite a newer authentication decision.
This coordination is per client instance, not across separate BFF requests.

`onTokenRefresh` may be asynchronous and call the same client. Requests already
awaiting its rotation still wait for the callback, so the callback must not
await a request that has already joined that rotation. A delayed old-token 401
returns its original error while that notification is pending or has failed
without a recovery in progress, so it cannot use credentials whose persistence
is uncertain. Callback failure
preserves the rotated credentials in memory. Requests made during notification
can use the current token, but a 401 does not start another refresh until the
notification finishes. This prevents overlapping persistence callbacks from
storing a superseded refresh token. A delayed 401 joins a newer in-flight
rotation when no notification is active.
If credentials can be replaced while a callback writes to external storage,
that storage must use an atomic conditional write or another ordering mechanism
to prevent an older callback from overwriting newer credentials. Checking
`getTokens()` before an asynchronous write alone is not sufficient.
The Web runtime client persists a captured pair only if it still matches that
instance's current credentials after asynchronous cookie acquisition.

The Web runtime's raw HTTP requests share this SDK refresh owner. Its API URL
is origin-relative: a pathname suffix is not a gateway prefix. Standalone SDK
`baseUrl` and `pathPrefix` routing are unchanged. Web refresh or notification
failure returns the original 401, which its typed adapter converts to a runtime
error; initial transport errors still propagate.

`fetchResponse(path, init, { auth })` returns an unconsumed response, including
non-2xx responses, without inferring a JSON content type. Its default auth is
`"optional"`; `"none"` suppresses default authorization and automatic refresh.
Explicit request headers still override defaults and are not automatically
refreshed using the client's credentials. Raw mutation requests are not retried
unless `retryUnauthorized` is explicitly enabled for an endpoint that rejects
authentication before admitting the operation. Streams and one-shot bodies
are never replayed after acceptance. `getTokens()` returns a
detached snapshot of current credentials, not the initial configuration.
Bodies must remain replayable for the single automatic retry.

Buffered `getRunEvents` uses the same authenticated request and structured
`AstraApiError` decoding as REST, including error codes and action hints.

Do not expose long-lived **refresh tokens** in browser-accessible JavaScript.
The dashboard (`web/`) keeps tokens in **httpOnly cookies** and routes browser
requests through explicit **Next.js Route Handlers** such as `/api/chats`,
`/api/models`, `/api/skills`, and `/api/chats/{id}/stream`. Those BFF handlers
construct an authenticated `AstraClient` server-side and call the runtime with
typed SDK methods. Avoid adding a generic browser-visible runtime proxy; add a
small typed BFF route instead when a new browser surface needs runtime access.

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
  baseUrl: 'http://localhost:17001',
  accessToken: token,
});

function Chat() {
  const {
    messages,
    sendMessage,
    isStreaming,
    plan,
    tasks,
    agents,
    toolCalls,
    usage,
  } = useAstraChat({
    client,
    agentId: 'optional-agent',
    model: 'optional-model',
    executionBudget: { initialTurns: 4, hardTurnLimit: 24 },
    capabilities: ['multi_agent', 'reflect'],
    allowTools: ['github'],
    explain: true,
  });

  return (
    <div>
      {messages.map((m) => (
        <div key={m.id}>{m.content}</div>
      ))}
      <aside>
        {tasks.map((task) => (
          <div key={task.id}>{task.title}: {task.status}</div>
        ))}
        {agents.map((agent) => (
          <div key={agent.agentId}>{agent.description}: {agent.status}</div>
        ))}
        {toolCalls.map((tool) => (
          <div key={tool.callId}>{tool.tool}: {tool.status}</div>
        ))}
      </aside>
      <button type="button" onClick={() => sendMessage('Hello!')}>
        Send
      </button>
    </div>
  );
}
```

`useAstraChat` projects task-board snapshots and agent lifecycle events into
stable `tasks` and `agents` arrays while retaining raw `agentEvents` for custom
audit or visualization needs. Integration boundaries such as
`agentBinding`, `runtimeProfile`, `executionBudget`, `capabilities`, `explain`,
`context`, `allowSkills`, `allowTools`, `workspaceBinding`, and
`executorBinding` are passed through the same typed chat request rather than
being flattened into prompt text. `workspaceBinding` uses the canonical request
shape (`kind` plus optional `display_name`, `root`, `source`, and `authority`);
the workspace object observed in runtime events is a separate response shape.

`cancelRun(runId)` returns the acknowledged Run's `runId`, `status`,
`executionSettled` and optional `contextNotSaved`. A successful command with an
invalid or missing receipt returns `null`; it must not be treated as a rejected
cancellation. An unsettled acknowledgement does not confirm Context saving.
The matching `run_finished.context_not_saved` field is `true` for lost Context,
`false` for saved/unchanged Context, and absent when unconfirmed. The chat hook
exposes this as `ChatMessage.contextNotSaved` (`null` means unconfirmed), and
`contextSaveWarning` provides the shared display text without changing the
assistant's content. Cold conversation items carry the same receipt projection;
a `cancellation` item represents a stopped Run without a visible assistant.

## §5.5 Edge protocol (from TypeScript)

When a **local edge** runs tools, use the same routes as `astra-thin-client`:

| Method | Client API | Route |
|--------|------------|--------|
| Tool result | `postToolResult(body, { edgeExecutorId })` | `POST /tools/result` + `X-Astra-Edge-Id` |
| Approval | `postApprovalRespond(body)` | `POST /approval/respond` |
| Register edge | `registerEdge(body, { edgeTransportId })` | `POST /agents/edge` |
| Heartbeat | `postEdgeHeartbeat(body, { edgeTransportId })` | `POST /agents/edge/heartbeat` |

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
| `listRuns` | `GET /runs?limit&after_updated_at&after_run_id` → `RunListResponse` (`runs` normalized to `RunStatus[]`, seek cursor preferred) |
| `getRunStatus` / `cancelRun` / `pauseRun` / `resumeRun` | `GET`/`DELETE`/`POST` under `/chat/runs/{id}` |
| `getRunEvents` | `GET /chat/runs/{id}/stream?last_index=` (buffered SSE parsed to `StreamEvent[]`) |
| `delegateRun` / `listDelegations` / `pauseDelegations` / `resumeDelegations` | Multi-agent: `POST …/delegate`, `GET …/delegations`, `POST …/delegations/pause` · `resume` |
| `streamChat` | `POST /chat/stream` (SSE) |
| `getSessionEvents` / `listEvents` / `getCausalChain` | `GET /events/session/{id}`, `GET /events?…`, `GET /events/causal-chain/{id}` |
| `getEdgesStatus` | `GET /edges/status` |
| `memoryStore` / `memorySearch` / … | `/memory/*` |
| `listSkills` | `GET /skills` (maps `skills[]` to `SkillInfo[]`) |
| §5.5 | `postToolResult`, `postApprovalRespond`, `registerEdge`, `postEdgeHeartbeat` |

### `AstraWebSocket`

| Method | Description |
|--------|-------------|
| `connect()` / `close()` | Lifecycle |
| `on` / `off` | Event subscription |
| `sendMessage` | Chat message (`session_id` in payload when given) |
| `approveToolCall` | Tool approval |

### Utilities

Both live streams and `parseSseDataEvents` accept LF, CRLF, and CR line endings.
Live parsing preserves events across split line endings and UTF-8 byte chunks;
multiple `data:` lines are joined with a newline. The SDK retains its tolerance
for a final JSON event without a trailing blank line.
The shared parser removes exactly one leading BOM for direct text, buffered
HTTP, and live streams. Raw-line callbacks include blank event separators.
Terminal events parsed during the final flush also suppress trailing socket
errors and reconnect attempts.

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

Vitest config: `vitest.config.ts`. Suites cover `AstraClient`, SSE/WebSocket helpers, React hooks, and **`paths`** (`buildQueryString`, `joinApiPath`, path encoders).

## License

Apache-2.0. See the repository's [LICENSE](../../LICENSE).
