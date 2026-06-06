# Astra Web Integration Kit

> **Status**: Draft v0.1
>
> **Date**: 2026-05-16
> **Scope**: TypeScript SDK, React headless integration, optional UI components, and web backend adapters for embedding Astra into external web systems.

## Summary

Astra should be easy to embed into other web products without forcing those
products to adopt Astra's full web application. External systems such as Moi may
want Astra's runtime capabilities while keeping their own navigation, visual
design, permission model, tenant model, and product-specific workflows.

This document proposes an integration kit with four composable layers:

1. **Core SDK**: protocol, clients, event parsers, reducers, controllers, and
   shared types.
2. **Headless React**: React hooks that expose state and actions without UI.
3. **UI Kit**: optional reusable UI building blocks for teams that want a fast
   default experience.
4. **Backend Adapters**: server-side integration helpers for authentication,
   BFF routes, stream proxying, identity mapping, and runtime context binding.

These layers are not a strict linear chain. The dependency graph is:

```text
External UI or Astra UI Kit
        |
        v
Headless React hooks
        |
        v
Chat transport abstraction
        |
        +---- direct runtime transport ----> Core SDK ----> Astra Runtime
        |
        +---- product BFF route -----------> Backend adapter
                                                  |
                                                  v
                                              Core SDK ----> Astra Runtime
```

The key architectural rule is:

> External systems may replace upper layers freely, but protocol semantics and
> runtime state transitions should be implemented once in the Core SDK and
> reused everywhere.

The frontend layers do not depend on a specific backend framework. They depend
on a transport interface. A transport may call Astra Runtime directly in
server-side or trusted contexts, or it may call a product BFF route in browser
contexts. Backend adapters help implement those BFF routes without copying
Astra web's route logic.

## Problem

The current repository already has a strong starting point:

- `packages/sdk` exposes `@astra/sdk` and `@astra/sdk/react`.
- `@astra/sdk` covers runtime REST APIs, SSE streaming, WebSocket transport,
  sessions, runs, skills, events, memory, delegation, edge protocol routes, and
  path helpers.
- `web/` uses a secure BFF pattern: browser requests go through Next.js route
  handlers, and runtime tokens stay in httpOnly cookies.

However, the current web product is not yet an integration kit:

- `web/components/app/*` is a product shell, not a reusable embed surface.
- Stream parsing, text delta merging, thinking tag splitting, and event-to-UI
  state mapping appear outside the SDK and can drift from other clients.
- External systems can call the raw API, but they still need to reimplement
  browser auth, stream proxying, event normalization, run state management,
  artifact handling, approvals, and error semantics.
- A product like Moi may reasonably choose to build its own UI. That should not
  imply rebuilding Astra's protocol and runtime semantics.

Without a clean integration kit, every embedding product will either:

- adopt Astra's full web app even when it does not fit its product model, or
- rewrite a large amount of runtime integration code and eventually diverge.

## Goals

- Make Astra embeddable as capabilities, not only as a full web application.
- Let products such as Moi choose their integration depth:
  - full UI blocks,
  - headless React state and actions,
  - backend adapters plus custom frontend,
  - raw SDK only.
- Keep protocol parsing, event reduction, state transitions, and runtime error
  semantics centralized.
- Avoid silent fallback behavior. If a required runtime capability is missing,
  the integration layer should surface a typed error instead of degrading
  invisibly.
- Keep UI optional and replaceable.
- Keep browser integrations secure by default: no long-lived refresh token in
  browser-accessible JavaScript.
- Make Astra's own `web/` product consume the same integration primitives that
  external products consume.

## Non-Goals

- This is not a proposal to force external systems to use Astra's visual design.
- This is not a proposal to expose a generic browser-visible runtime proxy.
  Browser access should go through typed BFF routes.
- This is not a replacement for Astra Runtime APIs. It is a TypeScript and web
  integration layer on top of those APIs.
- This document does not define a full cross-language SDK strategy. The TypeScript
  kit should still produce contracts that future Java, Go, or Python adapters can
  follow.

## Design Principles

### 1. Core Protocol Logic Lives Once

The following logic should be implemented in the Core SDK and reused by Astra
web, Moi, examples, tests, and adapters:

- SSE frame parsing.
- Stream event validation and normalization.
- Text delta merging.
- Thinking/reasoning extraction.
- Run/session state transitions.
- Tool call lifecycle state.
- Approval lifecycle state.
- Artifact lifecycle state.
- Runtime error normalization.
- Abort, stop, cancel, and retry semantics.

No product UI should need to understand raw event frames.

### 2. Headless First, UI Second

The default reusable frontend surface should be headless:

```tsx
const chat = useAstraChat({
  sessionId,
  client,
  model,
  context,
});
```

The hook returns state and actions. It does not render JSX, does not impose
layout, and does not require Tailwind, Radix, Next.js, or Astra's visual design.

### 3. UI Kit Is Optional

The UI Kit exists to reduce integration cost for teams that want default
components. It must not be required by the hooks or the Core SDK.

Moi can ignore the UI Kit and still use the headless hooks and backend adapters.

### 4. Browser Security Requires a Backend Boundary

Browser JavaScript should not hold long-lived refresh tokens. Browser products
should call their own backend/BFF routes, and those routes should call Astra
Runtime with server-side credentials or token exchange.

### 5. External Product Context Is Explicit

Integrations should pass product context through a typed mapping layer:

```ts
{
  source: "moi",
  tenantId: "...",
  userId: "...",
  projectId: "...",
  pageKind: "dashboard",
  entityId: "..."
}
```

The adapter should not infer product context from arbitrary request data.
Context fields should be allowlisted and auditable.

### 6. Fail Fast Instead of Silent Fallback

If a product asks for a capability that is unavailable, unsupported, or
misconfigured, the SDK or adapter should return a typed error. It should not
silently switch transport, drop fields, disable tools, or reinterpret runtime
events unless the behavior is explicitly configured and documented.

## Layer 1: Core SDK

### Package

Existing package:

```text
packages/sdk
@astra/sdk
```

The current package should remain the protocol source of truth. New capabilities
can be added as new exports and internal modules without changing the package
identity.

Recommended public entrypoints:

```text
@astra/sdk
@astra/sdk/react
```

Optional future entrypoints if the surface grows:

```text
@astra/sdk/protocol
@astra/sdk/controllers
```

Do not create a separate core package unless package size or release cadence
forces it. A single package with clean entrypoints is simpler for consumers.

### Responsibilities

The Core SDK owns:

- `AstraClient` for typed REST calls.
- `AstraWebSocket` for WebSocket transport.
- Path constants and path builders.
- Auth token injection and refresh support.
- Runtime type definitions.
- SSE frame parsing.
- Stream event normalization.
- Chat/run reducers.
- Framework-agnostic controllers.
- Error classes.
- Test fixtures for protocol events.

### Framework-Agnostic Controller

The controller is the reusable state machine under the React hook.

Sketch:

```ts
const controller = createAstraChatController({
  transport,
  sessionId,
  initialMessages,
  context,
});

const unsubscribe = controller.subscribe((state) => {
  console.log(state.messages, state.isStreaming);
});

await controller.sendMessage({
  content: "Analyze this workload",
  options: { model: "sonnet-4.6-adaptive", thinking: true },
});
```

The controller should be usable by:

- React hooks.
- Vue/Svelte adapters.
- CLI tools.
- Node services.
- Unit tests.

It should not import React.

### Canonical Chat State

The SDK should define a normalized state model:

```ts
type AstraChatControllerState = {
  sessionId: string | null;
  activeRunId: string | null;
  messages: AstraChatMessage[];
  pendingApprovals: AstraApprovalRequest[];
  artifacts: AstraArtifactRef[];
  toolCalls: AstraToolCall[];
  plan: AstraPlanState | null;
  usage: AstraTokenUsage;
  connectionState: "idle" | "connecting" | "open" | "reconnecting" | "closed";
  runState: "idle" | "streaming" | "waiting_for_approval" | "cancelling" | "complete" | "failed";
  error: AstraRuntimeError | null;
};
```

The exact field names can evolve, but the important part is that UI clients read
one normalized state rather than reprocessing raw events.

### Event Reducer

The reducer should accept normalized stream events and produce deterministic
state transitions:

```ts
const next = reduceAstraChatEvent(previous, event);
```

This reducer should be covered by fixture-driven tests:

- text streaming,
- reasoning streaming,
- thinking tags embedded in text,
- tool calls,
- approval waits,
- artifact emission,
- run success,
- run failure,
- cancellation,
- malformed event,
- reconnect/resume where supported.

### Transport Model

The SDK should support multiple transports behind a common interface:

```ts
type AstraChatTransport = {
  send(input: AstraChatSendInput, handlers: AstraStreamHandlers): AstraStreamHandle;
  cancel?(runId: string): Promise<void>;
};
```

Initial transport implementations:

- Direct runtime SSE transport using `AstraClient.streamChat`.
- BFF SSE transport using an application route such as
  `/api/astra/chats/{sessionId}/stream`.

The headless hook should not need to know whether it is talking directly to the
runtime or to a product BFF route.

### Core SDK Acceptance Criteria

- Astra web and external examples use the same parser and reducer.
- No duplicate implementation of SSE parsing or text delta merging remains in
  product code.
- Unit tests cover protocol edge cases with event fixtures.
- Typed errors are exposed for auth, missing runtime capability, protocol
  mismatch, stream failure, cancellation, and invalid response.

## Layer 2: Headless React

### Package Entry

Existing entrypoint:

```text
@astra/sdk/react
```

This should remain a thin React binding over the Core SDK controller.

### Responsibilities

Headless React owns:

- Creating and disposing controllers in React lifecycle.
- Subscribing controller state into React state.
- Returning stable action functions.
- Coordinating React-specific concerns such as suspense boundaries only if
  explicitly introduced later.

It does not own:

- protocol parsing,
- stream reduction,
- visual components,
- route handlers,
- product-specific persistence,
- tenant authorization,
- artifact download authorization.

### Primary Hook

Sketch:

```ts
const chat = useAstraChat({
  sessionId,
  transport,
  initialMessages,
  context: {
    source: "moi",
    tenantId,
    projectId,
  },
});
```

Returned shape:

```ts
type UseAstraChatReturn = AstraChatControllerState & {
  sendMessage(input: string | AstraChatSendInput): Promise<void>;
  stop(): Promise<void>;
  retry(messageId?: string): Promise<void>;
  approveToolCall(approvalId: string, input?: unknown): Promise<void>;
  rejectToolCall(approvalId: string, reason?: string): Promise<void>;
  setModel(model: string): void;
  reset(): void;
};
```

### Additional Hooks

The first release does not need every hook, but the target API should include:

- `useAstraChat`
- `useAstraRun`
- `useAstraApprovals`
- `useAstraArtifacts`
- `useAstraModels`
- `useAstraSkills`
- `useAstraSessions`

These should compose around the same core clients and controllers.

### Example Usage

```tsx
function MoiAstraPanel({ sessionId, projectId }: Props) {
  const chat = useAstraChat({
    sessionId,
    transport: createBffChatTransport({ basePath: "/api/moi/astra" }),
    context: {
      source: "moi",
      projectId,
    },
  });

  return (
    <>
      <MoiMessageList messages={chat.messages} />
      <MoiArtifactDrawer artifacts={chat.artifacts} />
      <MoiComposer
        disabled={chat.runState === "streaming"}
        onSubmit={(content) => chat.sendMessage(content)}
      />
    </>
  );
}
```

### Headless React Acceptance Criteria

- A React product can build a complete custom chat UI without importing
  `@astra/ui`.
- The hook can be tested with mocked transports and deterministic event
  fixtures.
- Hook behavior matches the controller behavior exactly.
- Astra web consumes the hook for its main chat path.

## Layer 3: UI Kit

### Package

Recommended package:

```text
@astra/ui
```

Possible location:

```text
packages/ui
```

This package is optional. It should depend on `@astra/sdk/react`, not the other
way around.

### Why This Layer Exists

Some products will want Astra capabilities quickly and will not want to rebuild
common agent UI patterns:

- composer,
- streaming message list,
- reasoning block,
- tool timeline,
- approval modal,
- artifact panel,
- skill picker,
- model switcher.

The UI Kit provides a fast path and an official reference interaction model.

It also prevents Astra's own web product from diverging from the external
integration surface.

### Responsibilities

The UI Kit owns reusable presentation components:

- `AstraChatPanel`
- `AstraMessageList`
- `AstraMessageBubble`
- `AstraComposer`
- `AstraReasoningBlock`
- `AstraToolTimeline`
- `AstraApprovalDialog`
- `AstraArtifactPanel`
- `AstraSkillPicker`
- `AstraModelSwitcher`
- `AstraRunStatus`

It does not own:

- runtime clients,
- auth,
- BFF routes,
- product navigation,
- product authorization,
- durable session storage.

### Composition Model

Components should work at two levels:

High-level panel:

```tsx
<AstraChatPanel
  chat={chat}
  slots={{
    Header: MoiPanelHeader,
    MessageActions: MoiMessageActions,
  }}
/>
```

Fine-grained blocks:

```tsx
<AstraMessageList messages={chat.messages} />
<AstraComposer disabled={chat.runState === "streaming"} onSubmit={chat.sendMessage} />
<AstraArtifactPanel artifacts={chat.artifacts} />
```

The high-level component is convenient. The fine-grained components keep the UI
Kit from becoming an all-or-nothing dependency.

### Theming

The UI Kit should support:

- CSS variables for color, radius, spacing, and typography.
- No hard dependency on Astra web's app shell.
- No hard dependency on Next.js routing.
- Minimal global CSS.
- Accessible keyboard behavior for composer, modals, menus, and approvals.

If the first implementation reuses existing `web/components/ui` primitives, they
should be moved or copied intentionally into `packages/ui` with stable public
APIs. Avoid importing from `web/*` because `web/` is a product, not a library.

### UI Kit Acceptance Criteria

- A new React app can render a working Astra chat panel with fewer than 50 lines
  of product code.
- A product can replace the message renderer and composer while keeping the rest
  of the panel.
- The UI Kit does not pull in Next.js as a runtime dependency.
- The UI Kit is optional for Moi-style custom integrations.

## Layer 4: Backend Adapters

### Package Options

Recommended initial packages:

```text
@astra/next
@astra/express
```

Possible locations:

```text
packages/next
packages/express
```

The first adapter should be `@astra/next` because Astra web already uses Next.js
route handlers and BFF patterns.

### Why This Layer Exists

External web systems already have their own:

- login,
- tenant model,
- role and permission model,
- project/resource ownership,
- audit requirements,
- backend framework,
- deployment topology.

They should not need to reimplement Astra-specific backend glue:

- token exchange,
- httpOnly cookie handling,
- runtime client construction,
- typed stream proxy,
- identity mapping,
- product context mapping,
- artifact download proxy,
- error normalization,
- request cancellation propagation.

### Security Boundary

The adapter is the recommended browser boundary:

```text
Browser UI
  -> Product BFF route
  -> Astra Runtime
```

The adapter should preserve these rules:

- No refresh token in browser-accessible JavaScript.
- Product backend authenticates the product user first.
- Product backend maps product user identity to Astra identity or service
  credentials.
- Product backend validates tenant/resource permissions before starting a run.
- Product backend passes only allowlisted context fields to Astra.
- Product backend controls artifact download authorization.

### Next.js Adapter Sketch

```ts
// app/api/astra/chats/[sessionId]/stream/route.ts
import { createAstraChatStreamRoute } from "@astra/next";

export const POST = createAstraChatStreamRoute({
  runtime: {
    baseUrl: process.env.ASTRA_RUNTIME_URL!,
  },
  async getUser(request) {
    return requireMoiUser(request);
  },
  async getAstraAuth(user) {
    return exchangeMoiUserForAstraToken(user);
  },
  async authorize({ user, params, body }) {
    await assertUserCanAccessMoiProject(user, body.context.projectId);
  },
  mapContext({ user, body }) {
    return {
      source: "moi",
      tenantId: user.tenantId,
      moiUserId: user.id,
      projectId: body.context.projectId,
    };
  },
});
```

The product supplies identity and authorization logic. The adapter supplies the
Astra protocol mechanics.

### Adapter Responsibilities

The adapter owns:

- Request parsing and validation.
- Runtime client construction.
- Server-side token refresh where configured.
- Streaming response framing.
- Cancellation propagation.
- Runtime error to product HTTP error mapping.
- Optional artifact download proxy helpers.
- Optional session route helpers.

The adapter does not own:

- product authentication,
- product authorization policy,
- tenant membership rules,
- business-specific context inference,
- UI behavior.

### Backend Adapter Acceptance Criteria

- A Next.js product can add a typed chat stream route without copying Astra web's
  route implementation.
- The adapter can be tested with a mocked runtime server.
- The adapter surfaces typed errors for missing auth, forbidden product access,
  runtime auth failure, runtime unavailable, and protocol mismatch.
- Astra web can migrate its BFF routes toward the adapter over time.

## Integration Modes

External systems should be able to choose one of four modes.

### Mode A: Full UI Embed

Use:

- `@astra/ui`
- `@astra/sdk/react`
- backend adapter

Best for:

- internal tools,
- prototypes,
- products that want an Astra panel quickly.

Product owns:

- where the panel appears,
- user and tenant auth,
- product context mapping.

### Mode B: Custom UI with Headless React

Use:

- `@astra/sdk/react`
- backend adapter

Best for:

- Moi-style product integration,
- systems with their own design system,
- systems with custom workflow around messages and artifacts.

Product owns:

- all visual components,
- layout,
- product-specific actions.

Astra owns:

- protocol,
- stream state,
- run state,
- approval state,
- artifact state.

### Mode C: Custom Frontend State with Backend Adapter

Use:

- backend adapter,
- `@astra/sdk` on the server,
- product's own frontend state management.

Best for:

- non-React frontend,
- existing complex state managers,
- frontend teams that want only a stable BFF contract.

### Mode D: Raw SDK

Use:

- `@astra/sdk`

Best for:

- server-side automation,
- CLI tools,
- tests,
- non-browser integrations,
- future language adapter reference implementations.

## Recommended Moi Integration Path

Moi likely should start with Mode B:

```text
Moi UI components
  -> @astra/sdk/react
  -> Moi BFF route built with @astra/next
  -> Astra Runtime
```

This gives Moi full control over product experience while avoiding duplicated
protocol and backend integration logic.

Moi should skip `@astra/ui` initially unless it wants a fast internal prototype.

## Runtime Context Contract

External context should be structured and explicit:

```ts
type AstraExternalContext = {
  source: string;
  tenantId?: string;
  userId?: string;
  projectId?: string;
  entityType?: string;
  entityId?: string;
  labels?: Record<string, string>;
};
```

Rules:

- Context fields must be serializable.
- Adapters should allowlist fields.
- Sensitive product data should not be forwarded by default.
- Context should be included in audit records where appropriate.
- Context should not be treated as authorization. Authorization happens before
  context is sent.

## Error Model

All layers should preserve typed errors.

Recommended categories:

- `AstraAuthRequiredError`
- `AstraForbiddenError`
- `AstraRuntimeUnavailableError`
- `AstraProtocolError`
- `AstraStreamError`
- `AstraCapabilityUnavailableError`
- `AstraValidationError`
- `AstraCancellationError`

Adapters may map these to HTTP status codes, but frontend hooks should still be
able to expose structured error information for UI decisions.

## Stop, Cancel, and Abort Semantics

This needs to be explicit because many web clients get it wrong.

Definitions:

- **Abort local stream**: stop reading the browser response. The backend run may
  continue unless cancellation is also requested.
- **Cancel run**: request Astra Runtime to stop the active run.
- **Stop UI generation**: product-level command that should usually cancel the
  run and then close the stream.

The controller should expose one high-level `stop()` action with documented
behavior. If a product needs lower-level control, it can use explicit methods:

```ts
chat.abortLocalStream();
chat.cancelRun();
```

No layer should silently treat local abort as runtime cancellation.

## Artifact Model

Artifacts are first-class runtime outputs, not message text decorations.

The SDK should normalize artifacts into:

```ts
type AstraArtifactRef = {
  id: string;
  kind: string;
  title?: string | null;
  filename?: string | null;
  contentType?: string | null;
  sizeBytes?: number | null;
  renderer?: string | null;
  downloadUrl?: string | null;
  createdAt?: string | null;
  metadata?: Record<string, unknown>;
};
```

Rules:

- Artifact listing and download authorization belong on the backend side.
- UI components can render artifact summaries and call product-provided download
  handlers.
- Runtime-specific artifact filtering should live in SDK/adapters, not in every
  product UI.

## Approval Model

Tool approvals and long-running waits should be normalized as state:

```ts
type AstraApprovalRequest = {
  id: string;
  runId: string;
  kind: string;
  title: string;
  description?: string;
  risk?: "low" | "medium" | "high";
  payload?: unknown;
};
```

The hook should expose:

```ts
approveToolCall(id, input?)
rejectToolCall(id, reason?)
```

The UI Kit may provide an approval dialog, but products can render approvals in
their own workflow surface.

## Package Dependency Rules

```text
@astra/sdk
  no React dependency

@astra/sdk/react
  depends on @astra/sdk
  peer depends on React

@astra/ui
  depends on @astra/sdk/react
  peer depends on React and React DOM
  no Next.js runtime dependency

@astra/next
  depends on @astra/sdk
  depends on Next.js types/runtime

@astra/express
  depends on @astra/sdk
  depends on Express types/runtime
```

Forbidden dependencies:

- `@astra/sdk` must not import React.
- `@astra/sdk` must not import Next.js.
- `@astra/ui` must not import from `web/*`.
- Backend adapters must not import UI packages.
- Product code should not import private SDK internals.

## Migration Plan

### Phase 0: Inventory and Contract Freeze

Deliverables:

- List duplicated protocol logic in `web/`.
- List current SDK stream event types.
- Define the first public controller state shape.
- Define adapter route contract for chat streaming.

Exit criteria:

- We know which current web logic will move into SDK.
- No package movement yet.

### Phase 1: Core Protocol and Reducer

Deliverables:

- Move SSE frame parsing into SDK.
- Move text delta merge into SDK.
- Move thinking/reasoning split into SDK.
- Add normalized stream event reducer.
- Add fixture-driven tests.

Exit criteria:

- Product code no longer implements protocol parsing manually.
- Current `web/` behavior is preserved through SDK tests.

### Phase 2: Framework-Agnostic Chat Controller

Deliverables:

- `createAstraChatController`.
- Transport abstraction.
- Direct runtime SSE transport.
- BFF SSE transport.
- Controller tests for send, stream, stop, error, and reset.

Exit criteria:

- Controller can run without React.
- React hook can become a thin wrapper.

### Phase 3: Headless React Hook

Deliverables:

- Rewrite `useAstraChat` around the controller.
- Add hooks for approvals/artifacts if needed by current web.
- Add React tests with mocked transport.

Exit criteria:

- Astra web can consume the hook for its chat flow.
- A custom UI example can consume the hook without `@astra/ui`.

### Phase 4: Next.js Backend Adapter

Deliverables:

- `createAstraChatStreamRoute`.
- Runtime client construction helper.
- Typed error mapping.
- Auth/context callback interface.
- Adapter tests with mocked runtime.

Exit criteria:

- A Next.js example can stream chat through the adapter.
- Astra web route logic can be simplified or partially migrated.

### Phase 5: Minimal UI Kit

Deliverables:

- `AstraMessageList`.
- `AstraMessageBubble`.
- `AstraComposer`.
- `AstraArtifactPanel`.
- `AstraApprovalDialog`.
- `AstraChatPanel` as a convenience composition.

Exit criteria:

- A demo app can use the full panel.
- Moi-style demo can use only headless hooks.

### Phase 6: Examples and Documentation

Deliverables:

- `examples/react-vite-headless`.
- `examples/next-bff-headless`.
- `examples/next-full-ui`.
- Migration guide for Astra web.
- Integration guide for external products.

Exit criteria:

- A new product engineer can integrate a working chat panel by following docs.

## Test Strategy

### Unit Tests

Core SDK:

- SSE parser fixtures.
- Event normalization.
- Event reducer transitions.
- Error normalization.
- Controller state transitions.

React:

- Hook lifecycle.
- send/stop/retry actions.
- mocked stream events.
- unmount cleanup.

Adapters:

- request validation.
- auth callback behavior.
- authorization callback behavior.
- context mapping.
- stream proxy behavior.
- error mapping.

### Integration Tests

- Mock runtime stream server.
- Next.js adapter route test.
- Browser fetch consuming BFF stream.
- Artifact list and download route tests.

### Example Smoke Tests

- Start example app.
- Send message.
- Receive streaming text.
- Receive reasoning.
- Receive artifact event.
- Cancel run.
- Surface runtime error.

## Versioning and Compatibility

- Use SemVer for public package APIs.
- Runtime event additions should be backward-compatible when possible.
- Unknown stream events should be preserved in an `unknownEvents` or callback
  path for observability, but should not mutate core state unless recognized.
- Breaking protocol changes require a documented migration path.
- Adapters should expose the runtime protocol version they expect.

## Documentation Requirements

Each package should include:

- Installation.
- Minimal example.
- Security notes.
- Error model.
- Testing guidance.
- Integration mode guidance.

The main integration guide should answer:

- "I want the whole chat panel. What do I import?"
- "I want my own UI. Which hook do I use?"
- "I use Next.js. How do I create the BFF route?"
- "I am not using React. What is the raw controller path?"
- "How do I pass product context safely?"
- "How do stop/cancel semantics work?"

## Anti-Patterns

Avoid:

- Copying Astra web route handlers into external products.
- Exposing refresh tokens to browser JavaScript.
- Adding a generic runtime proxy endpoint for all runtime paths.
- Reimplementing stream parsing in product UI.
- Letting UI components own runtime protocol logic.
- Making `@astra/ui` mandatory for React integrations.
- Importing from `web/*` inside packages.
- Silently falling back from one transport to another.
- Treating external context as authorization.

## Open Questions

1. Should `@astra/sdk/react` remain inside `@astra/sdk`, or should it become
   `@astra/react` before public release?
2. Which runtime event names are stable enough to document as public protocol?
3. Should the adapter own token exchange, or should it only accept a token
   provider callback?
4. What minimum artifact rendering should the UI Kit support in v1?
5. How much of Astra web's local chat persistence should remain product-specific
   versus move into reusable adapter helpers?

## Recommended Initial Decision

Start with the least disruptive path:

- Keep `@astra/sdk` as the package.
- Strengthen `@astra/sdk/react` as the headless React entrypoint.
- Add framework-agnostic controllers inside `@astra/sdk`.
- Build `@astra/next` after the controller and reducer stabilize.
- Add `@astra/ui` only after Astra web and one external-style example both use
  the headless layer successfully.

This order keeps the core clean, reduces duplication early, and avoids building
UI abstractions before the runtime integration contract is stable.
