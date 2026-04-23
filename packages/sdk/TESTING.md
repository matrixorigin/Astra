# Testing `@astra/sdk`

- **Test matrix (cases × offline vs online):** [TEST-MATRIX.md](./TEST-MATRIX.md)

## One-liners (local)

```bash
cd packages/sdk
npm install
npm run ci          # typecheck → test → build (matches CI)
npm test            # Jest only
npm run test:coverage
npm run typecheck
npm run build
```

**CI order:** `typecheck` first (fast fail on types), then `test`, then `build` (`tsup` + `.d.ts`). Use `npm run ci` so the order never drifts.

The GitHub **Static Checks** workflow (`.github/workflows/static-checks.yml`) runs a dedicated **`sdk` job** that executes `typecheck`, `test:coverage` (with threshold gates), and `build` in `packages/sdk`.

## Layers

| Layer | What | Where |
|-------|------|--------|
| L0 | Types + bundle | `tsc --noEmit`, `tsup` |
| L1 | Pure helpers | `read-http-error.test.ts`, `paths.test.ts`, `parseSseDataEvents` in `sse-client.test.ts` |
| L2 | HTTP + SSE with mock `fetch` | `client.test.ts`, `client-edge-stream.test.ts`, `sse-client.test.ts` |
| L3 | Long chains (no real server) | `__tests__/scenarios/real-world-scenarios.test.ts` |
| L4 | React hooks (jsdom) | `hooks.test.ts` |
| L5 | WebSocket | `websocket.test.ts` |

## Online smoke (optional)

Not run in default CI. Set `ASTRA_SDK_E2E=1` to enable Jest integration tests in [`src/__tests__/integration/online-smoke.test.ts`](src/__tests__/integration/online-smoke.test.ts).

**Mode A — local HTTP harness (no real Astra):** omit `ASTRA_SDK_BASE_URL`. Jest starts an in-process server ([`local-e2e-server.ts`](src/__tests__/integration/local-e2e-server.ts)) and runs real `fetch` + `AstraClient` against it: health, `login` + `getMe`, bearer `getMe`, `listSessions`, `getRunStatus`, `getRunEvents` (buffered SSE + `last_index`), and `pathPrefix: '/api'`.

**Mode B — your running API (real server):** set `ASTRA_SDK_BASE_URL` to the **HTTP API origin** (e.g. `http://127.0.0.1:PORT` from `make dev-start`). The harness (Mode A) is skipped. You need **either** a bearer token **or** username/password for authenticated API tests; otherwise only **GET /health** runs.

| Variable | Mode A (harness) | Mode B (remote) |
|----------|------------------|-----------------|
| `ASTRA_SDK_E2E` | `1` | `1` |
| `ASTRA_SDK_BASE_URL` | **unset** | **set** (no trailing path) |
| `ASTRA_SDK_ACCESS_TOKEN` | — | Optional; if set, used for all API calls |
| `ASTRA_SDK_USERNAME` + `ASTRA_SDK_PASSWORD` | — | Optional; if set (and no access token), `login` then API calls |
| `ASTRA_SDK_PATH_PREFIX` | — | Optional (e.g. `/api`); same meaning as `AstraClient` `pathPrefix`. Health stays `GET {base}/health` on the **root** origin. |
| `ASTRA_SDK_TEST_RUN_ID` | — | Optional; if set, runs `getRunStatus` and `getRunEvents` for that run id. |

**Health URL:** `GET {baseUrl}/health` is at the **server root**; it is not the same as routes under `pathPrefix`. The Jest suite and [`scripts/sdk-online-smoke.mjs`](./scripts/sdk-online-smoke.mjs) use this explicitly (see [TEST-MATRIX.md](./TEST-MATRIX.md)).

```bash
# Real fetch + in-process stub server (no Astra)
npm run test:integration:local
# equivalent: ASTRA_SDK_E2E=1 npm test -- --testPathPatterns=integration/online

# Jest + your real API (Mode B; **BASE_URL 默认 http://127.0.0.1:8000** 与 `make dev-start` 一致)
ASTRA_SDK_ACCESS_TOKEN=... npm run test:integration:remote
# 或: ASTRA_SDK_USERNAME=u ASTRA_SDK_PASSWORD=p  npm run test:integration:remote
# 非 8000 时:  ASTRA_SDK_BASE_URL=http://127.0.0.1:9xxx npm run test:integration:remote
# 可选: ASTRA_SDK_PATH_PREFIX=/api  ASTRA_SDK_TEST_RUN_ID=run-xxx

# Standalone script (no Jest; 默认同样指向 :8000)
npm run test:online
# 或: ASTRA_SDK_BASE_URL=http://127.0.0.1:9xxx npm run test:online
# Optional: ASTRA_SDK_ACCESS_TOKEN=... npm run test:online
```

**Repo `make test-online`:** that target runs **Rust** `#[ignore]` integration tests. SDK online tests are **Node-only** and use the env vars above.

## `AstraClient` method checklist

Use this when adding methods to [`src/client.ts`](src/client.ts); each should have at least one test (directly or via scenario).

- Auth: `register`, `login`, `logout`, `getMe`, `setTokens`
- Sessions: `createSession`, `getSession`, `listSessions`, `deleteSession`, `getSessionAudit`, `updateSession`, `closeSession`, `resumeSession`, `cancelSession`, `getSessionActivity`, `getSessionReflect`, `getSessionDecisionTrace`, `getSessionEvents`
- Runs / chat: `createRun`, `getRunStatus`, `cancelRun`, `pauseRun`, `resumeRun`, `getRunEvents`, `listRuns`, `delegateRun`, `listDelegations`, `pauseDelegations`, `resumeDelegations`, `streamChat`
- Memory: `memoryStore`, `memorySearch`, `memoryRetrieve`, `memoryPurge`
- Skills: `listSkills`, `registerSkill`, `publishSkill`, `getSkill`, `unpublishSkill`
- Events / edges: `listEvents`, `getCausalChain`, `getEdgesStatus`
- Thin / edge: `postToolResult`, `postApprovalRespond`, `registerEdge`, `postEdgeHeartbeat`, `getTaskLease`, `postTaskLeaseClaim`, `postTaskLeaseRelease`, `postTaskLeaseRenew`

Wire helper: `chatRequestToWire` — see `chatRequestToWire` tests in [`client.test.ts`](src/__tests__/client.test.ts).

## Coverage

`npm run test:coverage` enforces global thresholds in [`jest.config.mjs`](jest.config.mjs). Re-export files (`index.ts`, `react.ts`) are excluded from coverage collection.

## Contract with runtime

- Paths: keep in sync with `rust/crates/astra-thin-client/src/paths.rs` (see comment in [`src/paths.ts`](src/paths.ts)).
- Request bodies: `chatRequestToWire` and JSON field names (snake_case) should match the server’s expected JSON for `/chat/stream` and related routes.
