# Generation Quality — Implementation TODO

> **Created**: 2026-02-20  
> **Goal**: Close the gap between design docs and implementation, prioritized by impact on generation quality.

---

## P0: Safety Correctness

### 1. Firewall `mode="block"` fail-closed
**File**: `core/verification/firewall.py`  
**Problem**: 5 early-return paths all return `safe_to_deliver=True` regardless of mode. In `block` mode, infrastructure failures (snapshot load, claim extraction) should block delivery.  
**Fix**: Extracted `_degraded_result(mode, reason)` — returns `safe=False, confidence=0.0` in block mode, `safe=True, confidence=0.5` in warn mode. 3 early-return paths updated.  
**Status**: ✅ Done

### 2. `log_verification` SQL injection risk
**File**: `core/verification/firewall.py` → `log_verification()`  
**Problem**: Uses `%s` placeholder raw SQL while rest of codebase uses `sqlalchemy.text()` with `:param` style.  
**Fix**: Rewritten to `sqlalchemy.text()` with named parameters for both `hallucination_checks` and `claim_evidence` inserts.  
**Status**: ✅ Done

---

## P1: Generation Quality

### 3. Claim-type weighted confidence
**File**: `core/verification/firewall.py` → `verify_response()`  
**Problem**: `confidence = verified / total` treats all claims equally. A response with 9 verified numeric claims and 1 failed causal claim scores 0.9.  
**Fix**: Added `_CLAIM_WEIGHTS` dict and `_weighted_confidence()` classmethod. Weights: causal(1.0) > factual(0.8) > temporal(0.6) > numeric(0.5). Handles both LLM extractor types and regex extractor types.  
**Design ref**: trust-and-safety.md §3 "Claim-Type Weighting"  
**Status**: ✅ Done

### 4. `_simple_verify_claim` substring → semantic similarity
**File**: `core/verification/firewall.py` → `_simple_verify_claim()`  
**Problem**: `claim.value.lower() in context_text.lower()` misses paraphrases → false negatives → unnecessary low-confidence warnings → users learn to ignore warnings.  
**Fix**: Added embedding similarity fallback (cosine ≥ 0.75 → verified). Uses `EmbeddingService.embed_text` when available, gracefully degrades to substring-only when not.  
**Status**: ✅ Done

---

## P2: Design Alignment (docs updated, code pending)

### 5. Streaming verification (hybrid approach)
**Design ref**: trust-and-safety.md §2 "Streaming Verification (Roadmap)"  
**Scope**: Sentence-boundary NLI checks during streaming, inline warnings.  
**Status**: ⬜ Design only — implementation deferred until NLI model infrastructure available

### 6. CoT audit (AlignmentCheck)
**Design ref**: trust-and-safety.md §2 "Chain-of-Thought Audit (Roadmap)"  
**Scope**: Lightweight classifier on assistant CoT before tool execution.  
**Status**: ⬜ Design only — implementation deferred

---

## P3: Code Quality

### 7. Remove `_get_default_schema` hardcoded schemas
**File**: `core/skills/modern_selector.py` → `_get_default_schema()`  
**Problem**: 6 hardcoded skill schemas. New skills require code change.  
**Fix**: Replaced with generic `{"type": "object", "properties": {}, "required": []}`. Also fixed pre-existing bug: `get_skill()` → `get()` to match `SkillRegistry` API.  
**Note**: `test_skill_to_tool_schema` tests the removed hardcoded behavior — needs update.  
**Status**: ✅ Done

### 8. SkillIndex incremental update
**File**: `core/skills/skill_index.py`  
**Problem**: `build()` re-embeds all skills every time. No incremental add/remove.  
**Fix**: Add `add(skill)` / `remove(name)` methods. Rebuild only on registry change.  
**Status**: ⬜ TODO (low priority — current brute-force is fine for <100 skills)

---

## Completed (this session)

- ✅ `run_step` exhausted-rounds: added firewall verification
- ✅ `run_step_with_planning`: added user_event, context snapshot, RUN_STARTED, audit lineage
- ✅ trust-and-safety.md: added streaming verification roadmap, CoT audit roadmap, claim-type weighting, fail-open/closed policy, new references
- ✅ ChatLoop audit alignment table added to design doc
