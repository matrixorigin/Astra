---
inclusion: always
---

# Project Architecture Map

## High-Level Shape

```text
mo-dev-agent/
├── rust/
│   └── crates/
│       └── api-shell/
│           ├── src/
│           │   ├── lib.rs
│           │   ├── app_state.rs
│           │   ├── storage.rs
│           │   ├── admin.rs
│           │   ├── runs.rs
│           │   ├── auth/
│           │   ├── bridge/
│           │   ├── server/
│           │   └── turn/
│           └── tests/
├── scripts/
├── deployment/
├── skills/
├── tests/fixtures/
└── docs/
```

## API-Shell Domains

- `server/` - HTTP handlers, router assembly, request shaping
- `bridge/` - chat-turn bridge transport, SSE parsing, side effects
- `turn/` - turn-domain planning, persistence contracts, and support logic
- `auth/` - auth/session/admin services plus token/encryption helpers
- `app_state.rs` - service wiring and defaults
- `storage.rs` - MatrixOne connection and persistence helpers
- `runs.rs` - run lifecycle contracts and fallback behavior

## Mental Model

`api-shell` is the Rust HTTP/API shell for the platform, not a temporary Python wrapper. It is now the primary implementation surface for the server-side product.
