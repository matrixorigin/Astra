# MCP transport stack: `rmcp`, `reqwest`, and WebSockets

**Status:** decision record (system risk plan H6 / H7)  
**Date:** 2026-04-18

## Scope

Clarify how Model Context Protocol (MCP) traffic relates to general HTTP (`reqwest`) and WebSocket (`tokio-tungstenite`) usage so we do not duplicate clients, split TLS policy, or fork protocol handling ad hoc.

## Current architecture

- **MCP client (`astra-cli` `mcp_client`)** uses the **`rmcp`** crate: protocol types, `serve_client`, and transports such as stdio (`TokioChildProcess`) and streamable HTTP / WebSocket as exposed by `rmcp` for this workspace version.
- **REST and non-MCP HTTP** (GitHub tools, thin client, admin/runtime HTTP) use **`reqwest`**. That is intentional: MCP is not “just HTTP JSON”; mixing it with ad-hoc `reqwest` calls would duplicate auth, versioning, and error semantics.
- **`tokio-tungstenite`** appears in the workspace for **generic WebSocket** paths (e.g. runtime WS handler, bridges) that are **not** the MCP-over-WS client unless explicitly wired through `rmcp`’s own transport stack.

## Decision

1. **Single MCP implementation:** Keep **`rmcp`** as the only supported MCP client stack for astra. New MCP features (capabilities, sampling, roots) extend through `rmcp` types and transports.
2. **Do not replace `reqwest` with `rmcp` for REST:** `reqwest` remains the standard for RESTful APIs (GitHub, cloud HTTP, downloads). No requirement to route those through MCP.
3. **WebSockets:** Prefer **`rmcp`’s transport** for MCP-over-WS. Use **`tokio-tungstenite`** (or higher-level Axum WS) only for **non-MCP** protocols or legacy surfaces already implemented that way. If a future endpoint is “MCP over WS”, implement it by extending the existing `rmcp` client path—not a parallel hand-rolled MCP frame parser on raw tungstenite.
4. **Spikes / experiments:** Prototype alternative MCP crates only behind a feature flag or a short-lived branch; merging a second MCP stack requires an explicit RFC (dual maintenance, security review, CI matrix).

## Rationale

- **Security and correctness:** MCP framing, capability negotiation, and error mapping stay centralized in one maintained dependency.
- **Operational clarity:** TLS, timeouts, and retries for MCP follow `rmcp` + our wrapper code; REST follows `reqwest` policies—fewer overlapping knobs.
- **Dependency budget:** We avoid pulling a second full MCP implementation “because HTTP is familiar”; the cost is duplicated bug surface and drift.

## Follow-ups (non-blocking)

- When upgrading `rmcp`, run MCP integration tests (stdio + SSE/WS if enabled) and confirm transport feature flags still match `Cargo.toml` workspace pins.
- If upstream adds a first-class streamable-HTTP transport we need, prefer upgrading `rmcp` over vendoring protocol glue.
