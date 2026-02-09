# Deployment Architecture Proposal - Review Response

**Date**: 2026-02-10  
**Reviewer Feedback**: Skills system elevation and extensibility architecture  
**Status**: Incorporated into main proposal

---

## Summary of Changes

This document summarizes the key architectural enhancements made to the deployment proposal based on the review feedback regarding Skills system and extensibility.

---

## Core Insight from Review

> "This is not just about 'missing a Skills module' — it's about building a **living system** that can continuously absorb industry best practices."

The review correctly identified that the original proposal, while structurally sound, lacked explicit mechanisms for:
1. **Skills as first-class citizens** (not just implicit in tools)
2. **Extension system** for absorbing new practices without core refactoring
3. **Practice evolution workflow** (evaluate → sandbox → integrate → deprecate)

---

## Major Architectural Additions

### 1. Skills System (`core/skills/`)

**New Module Structure**:
```
core/skills/
├── skill.py           # Skill base class (input/output/version/safety)
├── registry.py        # Load from skills_registry table
├── composer.py        # Skill composition (chain/parallel/conditional)
├── filter.py          # Enhanced dynamic filtering (LLM-assisted)
└── validator.py       # Pre-call validation (permissions/params)
```

**Key Design**:
- Skills are **versioned data assets** (same level as `prompt_templates`)
- Skills reference **tools** (not replace them): `tools: ["github_api", "llm"]`
- Skills have **safety rules**: `["no_pii", "max_tokens=500"]`
- Skills are **composable**: sequential/parallel/conditional workflows

**Integration Point**:
```python
# core/context/builder.py modification
context_snapshot["skills_used"] = [
    {"skill_id": s.skill_id, "version": s.version} 
    for s in filtered_skills
]
```

### 2. Extension System (`core/extensions/` + `extensions/`)

**New Module Structure**:
```
core/extensions/
├── manager.py         # ExtensionManager (load/register/hooks)
└── base.py            # Extension base class

extensions/
├── registry.json      # Extension registry (metadata)
├── TEMPLATE.md        # New extension template
├── skills/            # Community/custom skills
├── workflows/         # Pre-built workflows
├── analytics/         # New analytics practices
└── guardrails/        # New safety practices
```

**Key Design**:
- **Core minimalism**: Core engine only provides extension points
- **Explicit extensions**: All new practices go through `extensions/` directory
- **Event hooks**: Extensions integrate via event listeners (no core modification)
- **Sandbox validation**: New extensions tested in sandbox before production

**Extension Manager API**:
```python
class ExtensionManager:
    def load_extension(self, path: str):
        """Dynamically load extension (no service restart)"""
        
    def register_hook(self, event_type: str, callback: Callable):
        """Register event hooks for extensions"""
        
    def trigger_hooks(self, event_type: str, event: Event):
        """Trigger all registered hooks for event type"""
```

### 3. Extension Evolution Workflow

**Process**:
```
Discover Practice → Evaluate → Test → Sandbox Validate → Merge/Discard
                                                ↓
                                    Monitor Effectiveness
                                                ↓
                                    Promote to Core / Deprecate
```

**Key Mechanisms**:
- **Extension Registry**: Unified management (`extensions/registry.json`)
- **Sandbox Validation**: Zero-risk testing on historical data
- **Deprecation Policy**: `@deprecated` decorator + migration guide
- **Practice Scorecard**: Call count / success rate / user satisfaction

### 4. Skill Effectiveness Closed Loop

**New Table**: `skill_evaluations`
```sql
CREATE TABLE skill_evaluations (
  evaluation_id VARCHAR(64) PRIMARY KEY,
  skill_id VARCHAR(64) NOT NULL,
  event_id VARCHAR(64) NOT NULL,
  success BOOLEAN,
  latency_ms INT,
  user_feedback TEXT,
  created_at TIMESTAMP
);
```

**Analytics**: `extensions/analytics/skill_effectiveness.py`
- Generate skill heatmap (success rate by skill)
- Auto-downgrade low-performing skills

---

## Updated Implementation Roadmap

### Phase 1.5: Skills System (Week 5) — **NEW**

**Goal**: Elevate skills to first-class citizens

**Deliverables**:
- [ ] Implement `core/skills/` module (skill.py, registry.py, composer.py, filter.py, validator.py)
- [ ] Implement `core/extensions/manager.py` (ExtensionManager)
- [ ] Create `extensions/` directory structure
- [ ] Create `extensions/registry.json` and `TEMPLATE.md`
- [ ] Modify `core/context/builder.py` to inject `skills_used` into `context_snapshot`

**Test**:
- [ ] `tests/unit/test_skill_composer.py` (verify skill composition logic)
- [ ] `tests/integration/test_extension_loading.py` (load extension from yaml/py)

### Phase 4: Experience + Analytics (Week 11-12) — **UPDATED**

**Added Deliverables**:
- [ ] Create example extensions:
  - [ ] `extensions/skills/github_issue_skill/`
  - [ ] `extensions/workflows/customer_refund_flow.yaml`
  - [ ] `extensions/guardrails/pii_detector.py`
- [ ] Implement Makefile commands: `make test-extension`, `make sandbox-validate`

**Added Test**:
- [ ] `tests/integration/test_extension_workflow.py` (load → validate → deploy extension)

---

## Updated Critical Success Metrics

**Added Metrics**:
- **Skill Success Rate**: >90% (successful skill calls / total skill calls)
- **Extension Adoption Rate**: Track monthly (new extensions added vs deprecated)

---

## Updated Alignment with Design Documents

**Added Rows**:
- **Skills as First-Class**: `core/skills/` + `skills_registry` table + `extensions/`
- **Extension System**: `core/extensions/manager.py` + `extensions/registry.json`

---

## Updated Risk Mitigation

**Added Risks**:
- **Extension fragmentation**: Mandatory Extension Registry + tag classification
- **Core bloat from extensions**: Core engine only provides extension points
- **Skill version conflicts**: Each skill declares dependency versions
- **Extension security vulnerabilities**: Extension sandbox execution

---

## Industry Best Practices Integrated

| Practice | Representative Solution | Integration Point |
|----------|------------------------|-------------------|
| **Skills as Code** | Semantic Kernel | `core/skills/` + Git management |
| **Skill Composition** | LangChain LCEL | `core/skills/composer.py` |
| **Skill Marketplace** | Dify Plugin Hub | `extensions/skills/` |
| **Skill Versioning** | Microsoft AutoGen | Same level as `prompt_templates` |
| **Skill Safety** | NVIDIA NeMo Guardrails | Enhanced `core/tools/sandbox_executor.py` |
| **Skill Discovery** | IBM Watsonx Orchestrate | Enhanced `core/skills/filter.py` |
| **Multi-Modal Skills** | Google Vertex AI Agents | Skill metadata extension |

---

## Key Architectural Philosophy

**Before**:
```
Core = Events + Context + Memory + Replay + Sandbox
```

**After**:
```
Core = Events + Context + Memory + Replay + Sandbox + Skills + Extensions
```

**Ultimate Goal**:
> "When tomorrow brings a disruptive practice, we can integrate it in 1 day, not refactor the entire system."

This transforms mo-dev-agent from an "excellent system" into an **"industry practice incubator"**.

---

## Open Questions Added

6. **Extension governance**: Who approves new extensions? What's the review process for community contributions?
7. **Skill marketplace**: Should we plan for external skill marketplace integration (e.g., Dify Plugin Hub)?

---

## Conclusion

The review feedback has been **fully incorporated** with the following impact:

1. **Skills elevated to first-class citizens**: No longer implicit in tools; explicit versioned data assets
2. **Extension system provides growth mechanism**: New practices can be integrated without core refactoring
3. **Practice evolution workflow defined**: Clear path from discovery to production or deprecation
4. **Industry best practices mapped**: Concrete integration points for Semantic Kernel, LangChain LCEL, etc.
5. **Timeline adjusted**: Added Phase 1.5 (1 week) for Skills system; total timeline now 15 weeks

**Impact on Core Design Principles**:
- ✅ Event-centric design: **Unchanged** (still core)
- ✅ Reproducibility: **Enhanced** (skills_used in context_snapshot)
- ✅ Data asset evolution: **Enhanced** (skills + extensions are data assets)
- ✅ Architectural growth: **NEW** (extension system enables continuous evolution)

The proposal now addresses both the **immediate need** (Skills module) and the **long-term vision** (living system that absorbs best practices).
