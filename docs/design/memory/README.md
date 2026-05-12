# Memory System Documentation

> **Canonical reference**: [../memory-runtime.md](../memory-runtime.md)

---

## Canonical docs (current, Rust runtime)

The `astra-engine` memory runtime is documented end-to-end in
[../memory-runtime.md](../memory-runtime.md). That single file covers:

- Session start → per-turn recall → session-end governance loop
- The `memory(action=...)` tool surface (9 cognitive verbs, schema, wire path)
- Write paths: LLM-driven `remember`, background extraction, session-end
- Read paths: session prewarm, `<memory_index>`, per-turn hybrid recall, LLM `recall` decoration
- Freshness buckets, trust tiers, cache lanes (stable vs volatile)
- Seen-ledger dedup (bridge + tool side)
- Debounced governance, scene forward-feeding, auto-snapshot safety net
- Team visibility via `astra:team:<id>` tag encoding
- Environment variables, troubleshooting, source pointers

See also:
- [../session-memory-protocol.md](../session-memory-protocol.md) —
  upstream L0/L1/L2 session memory pyramid (in-session context, distinct
  from cross-session Memoria storage).

---

## Legacy Python-era docs (historical reference only)

The files in *this* directory describe an earlier Python design
(tabular + graph backends, protocol-based factory, Python
`SessionSummarizer`). That architecture has been replaced by the
current Rust runtime. These files are retained for historical context
and for the ideas that carried over (tier-weighted decay, governance
scheduling, 3-phase reflect), but they do NOT reflect how
`astra-engine` works today:

| Legacy document | Status |
|---|---|
| [memory-overview.md](memory-overview.md) | Historical — Python-era 5-layer model |
| [tabular-memory.md](tabular-memory.md) | Historical — replaced by Memoria HTTP |
| [graph-memory.md](graph-memory.md) | Historical — graph backend not in Rust runtime |
| [intent-driven-loading.md](intent-driven-loading.md) | Historical — Python-era Tier 0/1 classification |
| [backend-coexistence.md](backend-coexistence.md) | Obsolete — factory model retired |
| [mo-memory-mcp.md](mo-memory-mcp.md) | Historical — Memoria MCP server docs |

For the current Memoria HTTP server integration, refer to the Memoria
project documentation directly.
