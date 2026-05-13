# Web Runtime Client / SDK Refactor TODO

Status: Step 1 complete; Step 2 path-parity first pass complete; Step 3 first pass complete for existing SDK coverage.

This refactor keeps the runtime protocol as HTTP REST + SSE + JSON. A JS/TS SDK is a client implementation of that protocol, not the protocol itself.

## Step 1 — Internal Typed Runtime Client

Goal: all Web server-side runtime calls go through one internal typed client before any public SDK work.

- [x] Add `web/lib/runtime-client/` as the Web runtime communication boundary.
- [x] Centralize runtime API URL, auth header construction, token refresh, JSON parsing, and structured runtime errors.
- [x] Remove the legacy generic `web/lib/api/client.ts`; browser code now talks to typed Web BFF routes.
- [x] Route Web chat store session/chat/run calls through the shared runtime client.
- [x] Route the chat SSE proxy's `/chat/stream` and artifact fetches through the shared runtime client.
- [x] Remove the generic `/api/backend/*` proxy after typed BFF routes covered the Web runtime surfaces.
- [x] Route auth-specific runtime calls (`/auth/login`, `/auth/register`, `/auth/refresh`, `/auth/logout`, `/auth/me`) through the shared runtime client while keeping the existing auth result interface.

Acceptance criteria:

- Web runtime calls outside browser-local Next API helpers use `web/lib/runtime-client`.
- Runtime errors preserve operation, path, HTTP status, and human detail.
- No runtime endpoint path or wire payload changes.

## Step 2 — Protocol Contract Stabilization

Goal: one canonical contract shared by Web SDK and Rust ThinClient.

- [x] Add TS SDK path helpers for models and Web-used session subresources (`state`, `transcript`, `artifacts`).
- [x] Add Rust ThinClient path helpers for Web-used session subresources that were only present in runtime routes.
- [x] Add path tests on both sides for session transcript/artifacts and model helpers.
- [x] Define first-pass canonical response DTO ownership in `@astra/sdk` for Web-used sessions, transcript, artifacts, models, skills, auth, chat response, and SSE `turn_complete.assistant_text`.
- [ ] Extend DTO ownership to remaining runtime surfaces not yet used by the Web UI.
- [ ] Fully align `packages/sdk/src/paths.ts` with `rust/crates/astra-thin-client/src/paths.rs`; current pass covers Web runtime calls only.
- [ ] Add generated or checked OpenAPI/JSON Schema artifacts for public runtime endpoints.
- [ ] Add contract tests that compare Rust and TS path/event schemas.

Acceptance criteria:

- CLI and Web clients consume the same endpoint names and event schemas.
- Contract drift fails CI.

## Step 3 — Public `@astra/sdk` Adoption

Goal: promote the internal client boundary into a reusable SDK surface.

- [x] Use existing `@astra/sdk` path constants/helpers for auth, chat, sessions, and run-stream calls in the Web runtime boundary.
- [x] Use the existing `@astra/sdk` buffered SSE parser for Web server-side run replay parsing.
- [x] Extend `@astra/sdk` path coverage for models and session subresources such as transcript/artifacts before replacing those remaining literals.
- [x] Move stable Web runtime-client behavior into `packages/sdk` where it belongs for shared HTTP helpers: error body parsing, header merging, JSON-capable method checks, and JWT subject extraction.
- [x] Add high-level SDK methods used by Web BFF flows: raw runtime session create/read/list/update, transcript paging, artifact listing, model listing, skill catalog listing, and non-streaming chat run creation with abort support.
- [x] Replace Web BFF session/transcript/artifact/model/skill-catalog/non-streaming chat calls with those SDK methods while keeping Next cookie refresh/writeback in `web/lib/runtime-client`.
- [ ] Move remaining stable typed runtime operations into `packages/sdk` when new Web surfaces need them; keep cookie/session concerns in the Web BFF.
- [ ] Keep Web-specific cookie/BFF behavior in `web/lib/runtime-client`.
- [x] Replace direct Web calls to internal wire helpers with `@astra/sdk` APIs where the SDK has sufficient coverage.
- [ ] Publish SDK docs for browser, Next.js BFF, Node, and tests.

Acceptance criteria:

- Web app uses the SDK for stable protocol operations.
- CLI keeps using Rust ThinClient, not JS.
- Runtime remains protocol-first: REST + SSE + JSON.
