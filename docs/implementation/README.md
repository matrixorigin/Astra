# Implementation Documentation Index

This directory contains detailed implementation documentation for astra-engine features.

---

## 📚 Core Features

### Tool Surface (ToolRegistry)
Unified tool surface with a stable always-load set, deferred manifest, and explicit `tool_search` activation.

- **Architecture:** [../design/tool-discovery-claude-code.md](../design/tool-discovery-claude-code.md)
- **Implementation:** `rust/crates/runtime/src/tool_registry/`

### Other Features

- **[memory-governance.md](memory-governance.md)** - Memory lifecycle governance & distributed scheduling ⭐
- **[authentication.md](authentication.md)** - JWT-based authentication
- **[llm-integration.md](llm-integration.md)** - LLM provider integration
- **[github-integration.md](github-integration.md)** - GitHub operations
- **[deployment.md](deployment.md)** - Deployment guide
- **[scope-configuration.md](scope-configuration.md)** - Configuration management
- **[ci.md](ci.md)** - CI/CD workflows

---

## 🗂️ Documentation Structure

```
docs/
├── design/                          # High-level design documents
│   ├── ARCHITECTURE.md
│   ├── memory-architecture.md
│   ├── trust-and-safety.md
│   ├── skills-and-tools.md          # Unified: skill architecture, ToolRegistry, marketplace
│   ├── agents-and-orchestration.md
│   ├── data-versioning.md
│   ├── evaluation-and-evolution.md
│
├── guides/                          # Usage guides
│
└── implementation/                  # Implementation details (this directory)
    ├── README.md                    # This file
    ├── memory-governance.md         # Memory lifecycle & distributed scheduling ⭐
    ├── authentication.md
    ├── llm-integration.md
    ├── github-integration.md
    ├── deployment.md
    ├── scope-configuration.md
    └── ci.md
```

---

## 📊 Current Status

| Feature | Status |
|---------|--------|
| Memory Governance | ✅ Stable |
| Tool Surface (ToolRegistry) | ✅ Stable |
| Authentication | ✅ Stable |
| LLM Integration | ✅ Stable |
| GitHub Integration | ✅ Stable |
| Deployment | ✅ Stable |

---

**Last Updated:** 2026-03-05
