# Tool Architecture Consolidation

## Problem

Adding or modifying a tool requires synchronized changes across 5-7 files in 4 crates.
No compile-time enforcement ensures consistency. Proven by `memory_search → memory_retrieve`
rename which took 5 commits to fully propagate, and the type mapping bug which was fixed in
one dispatch path but missed the other.

## Current Architecture (5 change points per tool)

```
             TOOL_CATALOG                 CLI Schema              Server Schema
         (tool_registry_meta.rs)     (edge_tools/schemas.rs)  (astra-tools/schemas.rs)
          triggers, intents,            JSON function def         JSON function def
          pinned, scope                 (different set!)          (different set!)
                 │                           │                         │
                 │                           │                         │
                 ▼                           ▼                         ▼
          Tool Selection              CLI Dispatch               Server Dispatch
        (tool_registry/scoring)    (edge_tools.rs:1778L)     (server_tool_executor:5290L)
                                    match arm per tool          match arm per tool
                                         │                          │
                                         │                          │
                                         ▼                          ▼
                                   CLI Memoria Client         Server Memoria Client
                                  (edge_tools/memoria:784L)  (astra-tools/memoria:551L)
                                   build_direct_request        build_direct_request
                                   cloud proxy dispatch        direct dispatch
                                   circuit breaker             circuit breaker
```

### Consequences

1. **Schema drift**: CLI and server tool lists diverge silently (86 vs 91 defs)
2. **Logic duplication**: `build_direct_request` duplicated with subtle differences
3. **Rename propagation**: 5+ files, no compiler help, found via session analysis
4. **Behavior inconsistency**: cloud proxy path vs direct path handle args differently
5. **Test duplication**: same contract tests written separately in both crates

## Proposed Architecture

### Principle: Tool Definition = Single Source of Truth

One struct per tool, co-located with its execution logic. Schema, triggers, dispatch, 
and type mapping all derived from the same definition.

### Phase 1: Unified Tool Schema (astra-tools)

Move ALL tool JSON schemas into `astra-tools`. Both CLI and server already depend on it.

```rust
// astra-tools/src/tool_defs.rs
pub struct ToolDef {
    pub name: &'static str,
    pub schema: serde_json::Value,      // JSON schema (single copy)
    pub triggers: &'static [&'static str],  // for selector scoring
    pub intents: &'static [IntentType],
    pub pinned: bool,
    pub scope: Scope,
}

pub static TOOL_DEFS: &[ToolDef] = &[
    ToolDef {
        name: "memory_store",
        schema: json!({ ... }),           // defined once
        triggers: &["remember", "记住", ...],
        intents: &[IntentType::Memory],
        pinned: true,
        scope: Scope::CrossSession,
    },
    // ...
];
```

**Eliminates**: CLI schemas.rs + server schemas.rs + TOOL_CATALOG duplication.
**Cost**: astra-turn-core would depend on astra-tools (or the shared definition moves to astra-core).

### Phase 2: Unified Memoria Client (astra-tools)

Merge CLI `edge_tools/memoria.rs` into `astra-tools::memoria::MemoriaClient`.

```rust
// astra-tools/src/memoria.rs
pub struct MemoriaClient {
    cloud_base: Option<String>,
    cloud_token: Option<String>,
    direct_base: String,
    direct_key: Option<String>,
    // single circuit breaker, single build_direct_request
}

impl MemoriaClient {
    pub async fn call(&self, op: &str, args: &Value) -> String {
        let args = self.normalize_args(op, args);  // type mapping here, once
        if let Some(ref cloud) = self.cloud_base {
            self.cloud_dispatch(cloud, op, &args).await
        } else {
            self.direct_dispatch(op, &args).await
        }
    }
}
```

**Eliminates**: 784L CLI client + 551L server client → one ~600L shared client.
**Cost**: CLI's `ToolExecutor` wraps `MemoriaClient` instead of reimplementing.

### Phase 3: Unified Tool Dispatch (astra-tools)

Each tool implements a trait:

```rust
pub trait ToolHandler: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&self, args: &Value, ctx: &ToolContext) -> ToolResult;
}
```

CLI and server both iterate `Vec<Box<dyn ToolHandler>>` instead of maintaining
parallel match arms.

**Eliminates**: CLI 72 match arms + server 73 match arms → single registration.
**Cost**: Larger refactor, trait-object overhead (negligible).

## Migration Path

| Phase | Files Eliminated | Risk | Effort |
|-------|-----------------|------|--------|
| 1. Unified schema | 2 files (~3700L → 1 file ~2000L) | Low | 2 days |
| 2. Unified Memoria client | 1 file (~784L → 0L, shared grows ~100L) | Medium | 2 days |
| 3. Unified dispatch | 2 match blocks (~145 arms → trait registration) | High | 4 days |

Recommend: Phase 1 + 2 on a dedicated branch. Phase 3 can be incremental.

## Evidence (bugs caused by duplication)

1. `memory_search` schema removed from CLI but not server (5 commits to fix)
2. Type mapping added to `build_direct_request` but missed cloud proxy path
3. `memory_retrieve` triggers defined in TOOL_CATALOG but not in CLI schema
4. System prompt mode detection checked `memory_search` after rename
5. Tool description said "Prefer memory_search" after removal
