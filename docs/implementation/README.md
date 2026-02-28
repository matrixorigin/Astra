# Implementation Documentation Index

This directory contains detailed implementation documentation for mo-agent-engine features.

---

## 📚 Core Features

### Self-Improving Selector
The breakthrough feature that enables automatic learning from failures.

- **[self-improving-selector.md](self-improving-selector.md)** - Implementation guide & roadmap ⭐

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
│   ├── skills-and-tools.md          # Unified: skill architecture, selection pipeline, marketplace
│   ├── agents-and-orchestration.md
│   ├── data-versioning.md
│   ├── evaluation-and-evolution.md
│   ├── learning-evolution-roadmap.md    # Evolution strategy
│
├── guides/                          # Usage guides
│   └── multi-dimensional-learning-guide.md
│
└── implementation/                  # Implementation details (this directory)
    ├── README.md                    # This file
    ├── memory-governance.md         # Memory lifecycle & distributed scheduling ⭐
    ├── self-improving-selector.md   # Implementation guide
    ├── authentication.md
    ├── llm-integration.md
    ├── github-integration.md
    ├── deployment.md
    ├── scope-configuration.md
    └── ci.md
```

---

## 🎯 Quick Links

### For Developers
- **Getting Started:** [../getting-started.md](../getting-started.md)
- **API Reference:** [../api-reference.md](../api-reference.md)
- **Development Guide:** [../development.md](../development.md)

### For Self-Improving Selector
- **Complete Guide:** [self-improving-selector.md](self-improving-selector.md) ⭐
- **Architecture:** [../design/skills-and-tools.md#3-skill-selection-pipeline](../design/skills-and-tools.md#3-skill-selection-pipeline)
- **Evolution Strategy:** [../design/learning-evolution-roadmap.md](../design/learning-evolution-roadmap.md)

---

## 📝 Document Types

### Implementation Guides
Detailed "how-to" documentation for implemented features:
- Architecture and design decisions
- Database schema
- Code examples
- Configuration
- Testing
- Monitoring

**Example:** `self-improving-selector.md`

### TODO & Roadmap
Future development plans with:
- Phase breakdown
- Task lists with effort estimates
- Dependencies
- Success metrics
- Technical debt tracking

**Example:** `self-improving-selector.md` ⭐

### Design Documents
High-level architecture and strategy:
- System design
- Trade-offs
- Evolution paths
- Best practices

**Location:** `docs/design/`

---

## 🔄 Document Lifecycle

1. **Design Phase** → Create design doc in `docs/design/`
2. **Implementation Phase** → Create implementation guide in `docs/implementation/`
3. **Planning Phase** → Create TODO doc with roadmap
4. **Maintenance Phase** → Update TODO as features are completed

---

## 📊 Current Status

| Feature | Implementation | TODO | Status |
|---------|---------------|------|--------|
| Memory Governance | ✅ Complete | - | Stable ⭐ |
| Self-Improving Selector | ✅ Complete | ✅ Complete | Phase 0 Done |
| Authentication | ✅ Complete | - | Stable |
| LLM Integration | ✅ Complete | - | Stable |
| GitHub Integration | ✅ Complete | - | Stable |
| Deployment | ✅ Complete | - | Stable |

---

## 🎯 Next Steps

1. **Review Guide:** [self-improving-selector.md](self-improving-selector.md)
2. **Start Phase 1:** Multi-Dimensional Learning (8 days effort)
3. **Update Progress:** Mark tasks as complete in TODO doc

---

**Last Updated:** 2026-02-20
