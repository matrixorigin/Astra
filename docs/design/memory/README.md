# Memory System Documentation

> Two memory backends, one protocol interface. Pick the right one for your use case.

---

## Architecture

```
                    ┌──────────────────────────┐
                    │   MemoryReader Protocol   │
                    │   MemoryWriter Protocol   │
                    │   MemoryAdmin Protocol    │
                    └────────────┬─────────────┘
                                 │
                    ┌────────────┴─────────────┐
                    │     create_memory_service │
                    │     (factory.py)          │
                    └────┬──────────────┬──────┘
                         │              │
              ┌──────────▼──┐    ┌──────▼──────────┐
              │   tabular   │    │      graph       │
              │  backend    │    │     backend      │
              │             │    │                  │
              │ flat table  │    │ directed graph   │
              │ vector+FTS  │    │ spreading activ. │
              │ observer    │    │ 3-phase reflect  │
              └─────────────┘    └─────────────────┘
```

## Documents

| Document | What It Covers | When to Read |
|---|---|---|
| [memory-overview.md](memory-overview.md) | Cognitive architecture, 5-layer model, context engineering, ownership/privacy, lifecycle governance, protocol interfaces | Understanding the overall memory design |
| [tabular-memory.md](tabular-memory.md) | `memories` table, vector+fulltext retrieval, observer, pollution detection, context snapshots, tool context engine | Working on the tabular backend (`core/memory/tabular/`) |
| [graph-memory.md](graph-memory.md) | `memory_graph_nodes`, spreading activation, 3-phase lifecycle (perceive/consolidate/reflect), tiered graph loading | Working on the graph backend (`core/memory/graph/`) |
| [intent-driven-loading.md](intent-driven-loading.md) | Task type → memory mode mapping, Tier 0/1 classification, token reduction | Working on context-layer memory loading optimization |
| [backend-coexistence.md](backend-coexistence.md) | Factory design, directory layout, migration path, testing strategy | Understanding how tabular/graph coexist |

## Quick Reference

| Aspect | Tabular Backend | Graph Backend |
|---|---|---|
| Data model | Flat rows in `memories` table | Typed directed graph in `memory_graph_nodes` |
| Retrieval | Vector similarity + fulltext + temporal + confidence | Spreading activation over graph edges |
| Write path | Observer → sensitivity filter → contradiction check → store | GraphBuilder → node + edge extraction → batch insert |
| Consolidation | SessionSummarizer (incremental + full) | 3-phase: perceive → consolidate → reflect |
| Reflection | ✅ Shared `ReflectionEngine` + `TabularCandidateProvider` | ✅ Shared `ReflectionEngine` + `GraphCandidateProvider` |
| Governance | Hourly/daily/weekly cleanup cycles | Same + orphan detection + edge pruning |
| Multi-hop | ❌ Single-hop vector search | ✅ 2-3 hop activation propagation |
| Maturity | ✅ Production (820+ tests) | 🔵 Design complete, implementation planned |
| Config key | `memory_backend = "tabular"` | `memory_backend = "graph"` |
