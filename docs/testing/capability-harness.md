# Astra capability harness

This harness is the deterministic layer below model-quality evaluation. It
checks the evidence and state transitions that must be correct regardless of
which model produced them. It deliberately does not compare assistant prose,
look for keywords, or encode one prompt as a product rule.

## What it covers

`crates/astra-harness/src/capability_matrix.rs` is the inventory and contract
checker. The inventory requires both a happy and an unhappy case for each
product quadrant and requires all three execution topologies:

| Quadrant | Typical boundary | Topologies represented |
| --- | --- | --- |
| Interaction | one current turn identity and one terminal event | CLI+Server, Edge+Server |
| Tool execution | runtime-bound schema, callback idempotency | CLI+Server, Edge+Server |
| Work and tasks | run/lease state transitions | Server-only, CLI+Server |
| Policy and approval | user/session/run-scoped decision | CLI+Server, Edge+Server |
| Delegation | bounded causal child runs | Server-only, Edge+Server |
| Context and cache | per-request accounting and safe cursors | CLI+Server, Server-only |
| Memory | optional work does not delay terminal response; user isolation | CLI+Server, Server-only |
| Observability | audit projection and typed failure evidence | Server-only, Edge+Server |
| Recovery | cancel/malformed callback are explicit terminal outcomes | CLI+Server, Edge+Server |
| Multi-tenant/performance | identity isolation under overlap | Server-only, Edge+Server |

The same module exposes `verify_trace_contract`, which checks a real or
recorded `SessionTrace` for:

- stable record/snapshot identity, monotonic turn order, causal time, and
  session counters (with per-turn counters compared only within the same turn);
- `context_total_tokens <= context_budget_tokens` when both are known;
- each LLM response and tool-batch completion has an earlier unmatched request
  or admission hook in the same turn;
- one initial `SessionStart`, a terminal `SessionEnd`, matching lifecycle
  timestamps, and `total_turns` derived from retained records;
- explicit completed state/final-text evidence and explicit interrupted
  state/interruption-kind evidence;
- zero silently-evicted records when the trace is used as whole-session proof.

`RecordingKernel` derives completed/interrupted trace outcomes from the typed
`SessionEnd` snapshot and counts any records evicted by its bounded buffer.
Therefore a truncated trace fails closed instead of looking like complete
evidence, while local checks remain independent of assistant prose.

These checks are typed snapshot/event checks. A model can phrase an answer in
any language and the checks remain unchanged.

## Fast validation

Run the harness and the directly affected contracts without starting services:

```bash
cargo test -p astra-harness capability_matrix
cargo test -p astra-turn-core artifact_windows
cargo test -p astra-turn-core cloud::tool_delivery
cargo test -p astra-services session_audit
cargo test -p astra-cli first_class_browser_surface
```

The capability inventory is intentionally cheap enough for every change. It
does not replace system tests; each case points to its focused or HTTP matrix
test. For a real MatrixOne + HTTP run, use the existing system matrix command
and select the named journey rather than running the entire suite after every
edit:

```bash
ASTRA_TEST_DB_IT=1 \
cargo test -p astra-runtime --test system_matrix_http_e2e \
  --features bridge-e2e-hooks --ignored e2e_matrix_cli_bridge_session_views_remain_consistent
```

Nightly/PR matrix tests remain the authority for auth, persistence, HTTP
boundaries, cross-user isolation, and concurrent Edge callbacks. The harness
inventory prevents a new capability from being considered complete unless it
also adds an unhappy-path contract and names the system-level proof.
