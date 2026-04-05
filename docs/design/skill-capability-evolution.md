# Skill Capability Evolution: From Prompt to Verifiable Capability

> **Status**: Design — addressing 3 fundamental architectural gaps  
> **Created**: 2026-07-10  
> **Related**: [skills-and-tools.md](skills-and-tools.md), [durable-agent-runs.md](durable-agent-runs.md)

---

## The Problem

Current skills are **prompt-native**: a skill is fundamentally a markdown template injected into context. This creates three cascading limitations:

1. **Not verifiable** — no way to know if a skill succeeded beyond "the LLM said it did"
2. **Not optimizable** — no feedback loop from execution to selection/improvement
3. **Not composable** — skills can't reliably chain because there's no contract between them

Claude Code has the same limitation. This document designs the path beyond it.

---

## 1. Skill as Capability Unit

### Current State: Skill = Instruction Text

```
User request → Skill selected → Markdown injected → LLM follows (maybe) → Output (unverified)
```

A skill today declares:
- What it's about (`description`, `when_to_use`, `tags`)
- What tools it may use (`allowed_tools`)
- How to execute (`execution_context: inline | fork`)
- Instructions (SKILL.md content)

It does NOT declare:
- What it **guarantees** (success criteria)
- What it **needs** as input (typed schema)
- What it **produces** as output (typed schema)
- How to **verify** it worked
- How it **composes** with other skills

### Target State: Skill = (Instruction + Tool Graph + State Transformation)

```
User request
  → Skill selected (with quality history)
  → Pre-check: required tools available? Input valid?
  → Execute (instruction + tool access)
  → Post-check: verification criteria met?
  → Record outcome → Feed back to selection
```

A capability-based skill additionally declares:
- **Input schema** — structured parameters with types and validation
- **Output schema** — what the skill produces (verifiable shape)
- **Success criteria** — machine-executable verification (reuses existing `VerifierKind`)
- **Required capabilities** — what tools/features the skill needs to function
- **Composition contract** — can it be called by other skills? Is it idempotent?

---

## 2. Manifest Extensions

### 2.1 Input/Output Schema

```yaml
name: security-scan
version: "1.2.0"

# NEW: Typed input (JSON Schema subset)
input_schema:
  properties:
    target_path:
      type: string
      description: "File or directory to scan"
      default: "."
    severity_threshold:
      type: string
      enum: [low, medium, high, critical]
      default: medium
  required: [target_path]

# NEW: Typed output
output_schema:
  properties:
    findings:
      type: array
      items:
        type: object
        properties:
          file: { type: string }
          line: { type: integer }
          severity: { type: string }
          description: { type: string }
    summary:
      type: string
    exit_code:
      type: integer
```

**Design decisions**:
- Schemas are **optional** — existing skills without schemas continue to work
- Use JSON Schema subset (not full spec) — `type`, `properties`, `required`, `enum`, `default`
- Input schema → argument validation before execution
- Output schema → result parsing after execution (best-effort, not strict)

### 2.2 Success Criteria

Reuse the existing `VerifierKind` from `durable_task.rs` (8 types already implemented):

```yaml
# NEW: Machine-executable success checks
success_criteria:
  - name: no_critical_findings
    verifier:
      kind: command_output
      cmd: "grep -c 'CRITICAL' {output_file}"
      contains: ["0"]
    required: true

  - name: report_generated
    verifier:
      kind: file_exists
      paths: ["{work_dir}/security-report.md"]
    required: true

  - name: quality_check
    verifier:
      kind: llm_judge
      prompt: "Did the security scan cover all files in the target path and report findings with actionable remediation?"
      pass_threshold: 0.8
    required: false  # advisory, not blocking
```

**Verifier types available** (from `durable_task.rs:702-746`):

| Kind | Description | Example |
|------|-------------|---------|
| `command` | Exit code check | `cargo test` exits 0 |
| `command_output` | Output pattern match | Output contains "PASSED" |
| `file_exists` | File presence check | `report.md` exists |
| `grep_check` | Pattern in file | File contains expected content |
| `build_pass` | Build succeeds | `cargo build` exits 0 |
| `test_pass` | Tests pass with min rate | 95% tests pass |
| `llm_judge` | Semantic evaluation | "Does output follow SOLID?" |
| `composite` | AND/OR of above | All of: tests pass AND builds |

**Key insight**: We don't need to invent a new verification system. The durable task verification engine (`VerificationRunner`) already handles all 8 types. We just need to make it available at skill level.

### 2.3 Required Capabilities

```yaml
# NEW: What this skill needs to function
required_capabilities:
  - capability: shell_execution
    reason: "Runs security scanning tools"
  - capability: file_read
    reason: "Reads source files for analysis"
  - capability: file_write
    reason: "Generates security report"

# Existing (enhanced): Tool requirements with features
required_tools:
  - name: bash
    version: ">=1.0"
  - name: read_file
  - name: write_file
```

**Capability vs Tool**: Capabilities are abstract ("shell_execution"), tools are concrete ("bash"). This allows:
- Skills to work across different tool sets (MCP tools that provide same capability)
- Validation: "can this environment run this skill?"
- Marketplace compatibility checking

### 2.4 Composition Contract

```yaml
# NEW: How this skill interacts with others
composition:
  composable: true           # Can be called by other skills
  idempotent: false          # Running twice may produce different results
  side_effects: [filesystem] # What external state it modifies
  max_duration_sec: 120      # Timeout for composition orchestrators
```

---

## 3. Learning Loop (Skill Quality Feedback)

### Current Gap

```
Tool execution → ToolQualityTracker → boost/penalize tool selection ✅
Skill execution → ??? → ??? ❌
```

Tools have a quality feedback loop. Skills don't. This means:
- A skill that fails 80% of the time is selected as often as one that succeeds 95%
- No data to improve skill instructions
- No way to A/B test skill versions

### Design: SkillQualityTracker

```rust
// New: rust/crates/runtime/src/skills/quality.rs

/// Mirrors ToolQualityTracker pattern from tool_registry/report.rs
pub struct SkillQualityTracker {
    metrics: HashMap<String, SkillMetrics>,
}

pub struct SkillMetrics {
    pub invocations: u32,
    pub successes: u32,          // verification passed
    pub failures: u32,           // verification failed
    pub partial: u32,            // some criteria passed
    pub avg_tokens: f32,         // resource consumption
    pub avg_turns: f32,          // fork execution turns
    pub avg_duration_ms: f32,    // wall clock time
    pub user_satisfaction: f32,  // explicit feedback (thumbs up/down)
    pub last_invoked: DateTime<Utc>,
}

impl SkillMetrics {
    /// Quality score [0.0, 1.0] — weighted success rate + user satisfaction
    pub fn quality_score(&self) -> f32 {
        let total = self.successes + self.failures + self.partial;
        if total == 0 { return 0.5; } // unknown = neutral
        let success_rate = self.successes as f32 / total as f32;
        let partial_credit = self.partial as f32 * 0.5 / total as f32;
        // 70% objective success, 30% user satisfaction
        (success_rate + partial_credit) * 0.7 + self.user_satisfaction * 0.3
    }

    /// Boost factor for skill selection [0.5, 1.5]
    pub fn selection_boost(&self) -> f32 {
        0.5 + self.quality_score()
    }
}
```

### Feedback Collection Points

```
Skill invoked
  │
  ├── Fork execution: SubRunResult has tokens, turns, success flag
  │     → Record: invocation, tokens, turns, duration
  │
  ├── Success criteria (if declared):
  │     → Run VerificationRunner on criteria
  │     → Record: success/failure/partial per criterion
  │
  ├── Implicit signals (no criteria declared):
  │     → Did user ask to retry? (failure signal)
  │     → Did user continue with result? (success signal)
  │     → Did user undo changes? (failure signal)
  │
  └── Explicit feedback:
        → /skill feedback <name> 👍👎
        → Record: user_satisfaction
```

### Learning Loop Integration

```
                    ┌─────────────────────────┐
                    │   SkillQualityTracker    │
                    │  (per-skill metrics)     │
                    └──────┬──────────────┬────┘
                           │              │
                    ┌──────▼──────┐  ┌────▼────────────┐
                    │  Selection  │  │  Reporting       │
                    │  Boost      │  │  /skill stats    │
                    └──────┬──────┘  └──────────────────┘
                           │
                    ┌──────▼──────────────────┐
                    │  format_skills_budget()  │
                    │  Higher quality = higher │
                    │  priority in listing     │
                    └─────────────────────────┘
```

**Impact**: Skills with proven track records get listed first in the budget-limited skill injection. Poor-performing skills naturally drop below the budget line.

---

## 4. Marketplace with Network Effects

### Current Gap

The current marketplace design is storage-only:
```
Stage (S3) → download → register → use
```

This is a CDN, not a marketplace. Missing:
- **Ranking**: Which skills are good?
- **Usage stats**: Which skills are popular?
- **Trust**: Who published this? Is it safe?
- **Compatibility**: Will this work with my setup?

### Design: Marketplace Signals

#### 4.1 Skill Metadata (Published with skill)

```yaml
# marketplace_metadata (appended during publish)
publisher:
  account_id: "org-matrixorigin"
  verified: true
  published_at: "2026-07-10T00:00:00Z"

compatibility:
  min_runtime_version: "0.9.0"
  required_capabilities: [shell_execution, file_read]
  tested_models: ["claude-sonnet-4-20250514", "gpt-4.1"]
  platforms: [linux, macos]
```

#### 4.2 Aggregate Signals (Stored in MatrixOne)

```sql
CREATE TABLE skill_marketplace_stats (
    skill_id        VARCHAR(255) PRIMARY KEY,
    publisher_id    VARCHAR(255) NOT NULL,
    total_installs  BIGINT DEFAULT 0,
    active_users_7d INT DEFAULT 0,
    avg_quality     FLOAT DEFAULT 0.0,   -- aggregated from user SkillQualityTrackers
    avg_rating      FLOAT DEFAULT 0.0,   -- explicit user ratings
    report_count    INT DEFAULT 0,       -- abuse/quality reports
    compatibility_score FLOAT DEFAULT 0.0, -- % of environments where it works
    last_updated    TIMESTAMP,

    INDEX idx_ranking (avg_quality DESC, active_users_7d DESC)
);

-- Per-user anonymous quality upload (opt-in)
CREATE TABLE skill_quality_reports (
    id              BIGINT AUTO_INCREMENT PRIMARY KEY,
    skill_id        VARCHAR(255) NOT NULL,
    skill_version   VARCHAR(50) NOT NULL,
    runtime_version VARCHAR(50) NOT NULL,
    success_rate    FLOAT,          -- from local SkillQualityTracker
    avg_tokens      FLOAT,
    invocation_count INT,
    reported_at     TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
```

#### 4.3 Trust Tiers

| Tier | Who | Trust Level | Verification |
|------|-----|-------------|-------------|
| **Bundled** | Platform team | Full trust | Built-in, tested in CI |
| **Verified Publisher** | Approved orgs | High trust | Code review + automated scan |
| **Community** | Any user | Medium trust | Automated scan only |
| **Unverified** | Anonymous | Low trust | User accepts risk |

Trust affects:
- Default permission level (verified → auto-approve tools; unverified → prompt each time)
- Marketplace ranking (verified publishers rank higher)
- Skill budget priority (higher trust → more likely to fit in budget)

#### 4.4 Ranking Algorithm

```
score = 0.35 * quality_score        -- aggregated success rate
      + 0.25 * popularity_score     -- log(active_users_7d) / log(max_users)
      + 0.20 * freshness_score      -- decay(days_since_update)
      + 0.15 * trust_score          -- tier weight (bundled=1.0, verified=0.8, community=0.5, unverified=0.2)
      + 0.05 * compatibility_score  -- % environments where it works
```

**Key difference from app stores**: Quality is measured **objectively** via SkillQualityTracker aggregation, not just ratings. A skill that claims to "write tests" but whose test_pass verification fails 60% of the time will rank poorly regardless of ratings.

#### 4.5 Network Effect Flywheel

```
More users → More quality data → Better ranking
  ↑                                      │
  └── Better skills surface first ←──────┘
```

The quality feedback loop (§3) is what makes the marketplace a marketplace rather than just storage. Without it, ranking is impossible.

---

## 5. Implementation Roadmap

### Phase 0: Foundation (prerequisite for all three)

**SkillExecutionResult enhancement** — extend the current 4-field result:

```rust
// Current (traits.rs:41-52)
pub struct SkillExecutionResult {
    pub output: String,
    pub tokens_used: u32,
    pub turns: u32,
    pub success: bool,
}

// Enhanced
pub struct SkillExecutionResult {
    pub output: String,
    pub tokens_used: u32,
    pub turns: u32,
    pub duration_ms: u64,
    pub success: bool,
    pub verification_results: Vec<CriterionResult>,  // per-criterion pass/fail
    pub error_category: Option<SkillErrorKind>,       // structured error
}
```

**Files**: `runtime/src/skills/traits.rs`, `runtime/src/skills/executor/`

### Phase 1: Verifiable Skills (问题1核心)

1. Add `input_schema` and `output_schema` to `SkillManifest`
2. Add `success_criteria: Vec<VerificationCriterion>` to `SkillManifest`
3. Create `SkillVerifier` that delegates to existing `VerificationRunner`
4. Wire verification into `partition_and_execute_skills()` post-execution
5. Update SKILL.md frontmatter parser for new fields

**Key reuse**: `VerifierKind` enum + `VerificationRunner` from `services/src/durable_task.rs` — zero new verification logic needed.

**Files**: `runtime/src/skills/manifest.rs`, `runtime/src/skills/verify.rs` (new), `runtime/src/turn/skill_tool.rs`

### Phase 2: Learning Loop (问题2核心)

1. Create `SkillQualityTracker` (mirrors `ToolQualityTracker` pattern)
2. Collect metrics from verification results + implicit signals
3. Wire quality scores into `format_skills_within_budget()` for selection boost
4. Add `/skill stats [name]` CLI command
5. Persist metrics (local config file → later DB)

**Files**: `runtime/src/skills/quality.rs` (new), `runtime/src/turn/skill_tool.rs`, `mo-agent/src/mo_agent/slash_skill.rs`

### Phase 3: Marketplace Signals (问题3核心)

1. Extend `skill_marketplace_index` table with quality aggregation columns
2. Anonymous quality report upload (opt-in)
3. Trust tier system in publisher metadata
4. Ranking algorithm in `/skill search` (remote mode)
5. Compatibility check during install

**Files**: `services/src/skills.rs`, `runtime/src/skills/marketplace.rs` (new)

### Phase 4: Composition (问题1延伸)

1. Add `composition` section to manifest
2. Skill-calls-skill via nested `skill` tool invocation
3. Schema compatibility validation (output of A matches input of B)
4. Orchestration primitives (sequence, parallel, conditional)

**Files**: `runtime/src/skills/composition.rs` (new), `runtime/src/turn/skill_tool.rs`

---

## 6. Design Decisions

### Q1: Should verification be blocking or advisory?

**Answer**: Configurable per criterion. `required: true` criteria block (skill marked failed). `required: false` criteria are advisory (logged, affects quality score, but skill output still returned).

**Rationale**: Strict verification breaks exploration use cases. Advisory verification enables learning without blocking.

### Q2: Should input/output schemas be strict?

**Answer**: Input validation is strict (reject bad input before execution). Output validation is best-effort (parse what you can, don't fail on unexpected shape).

**Rationale**: Input errors waste tokens. Output is LLM-generated and inherently approximate.

### Q3: Where does quality data live?

**Answer**: Phase 1-2: local config file (`~/.astra/skill_quality.json`). Phase 3: MatrixOne DB (enables aggregation across users).

**Rationale**: Local-first avoids infrastructure dependency. DB enables network effects later.

### Q4: Can existing skills use verification without changes?

**Answer**: Yes. Skills without `success_criteria` continue to work as today. Quality tracking still captures implicit signals (retry, undo, continuation).

**Rationale**: Backward compatibility is critical for adoption. Zero-config baseline must work.

### Q5: How does this relate to the Pin mechanism?

**Answer**: Orthogonal. Pin controls **whether** a skill is in the context. Quality controls **where** it ranks within the budget. A pinned skill with poor quality still appears (it's pinned) but gets a warning. An unpinned skill with great quality ranks higher in budget allocation.

---

## 7. Comparison: Current → Target

| Aspect | Current | Phase 1 | Phase 2 | Phase 3 |
|--------|---------|---------|---------|---------|
| **Nature** | Prompt template | Verifiable capability | Self-improving | Ecosystem participant |
| **Success** | LLM says so | Machine-verified | Tracked over time | Aggregated across users |
| **Selection** | Token budget | + schema validation | + quality boost | + marketplace ranking |
| **Composition** | Independent | Input/output typed | Quality-aware chaining | Marketplace dependencies |
| **Feedback** | None | Per-execution | Historical trends | Community signals |

---

## 8. What This Enables (Not Possible Today)

1. **"Show me skills that actually work for code review"** — ranking by verified success rate
2. **"This skill broke after model update"** — quality regression detection
3. **"Run security scan then test gen"** — typed composition with schema validation
4. **"Install the top-rated Python testing skill"** — marketplace with objective quality
5. **"Why did this skill fail?"** — structured error + criterion-level diagnostics
6. **"A/B test two skill versions"** — quality metrics comparison

---

## Appendix A: Reusable Components

### From durable_task.rs (already implemented)

| Component | Location | Reuse for Skills |
|-----------|----------|-----------------|
| `VerifierKind` enum (8 types) | `services/src/durable_task.rs:702` | Skill success criteria |
| `VerificationCriterion` struct | `services/src/durable_task.rs:676` | Skill verification config |
| `VerificationRunner` | `services/src/durable_task.rs:770` | Execute skill verification |
| `parse_acceptance_to_criteria()` | `services/src/contract_generator.rs:145` | Auto-detect criteria from text |

### From tool_registry (pattern to mirror)

| Component | Location | Mirror for Skills |
|-----------|----------|------------------|
| `ToolQualityTracker` | `runtime/src/tool_registry/report.rs:58` | → `SkillQualityTracker` |
| `SelectionReport` | `runtime/src/tool_registry/report.rs:15` | → `SkillSelectionReport` |
| `SelectionFeedback` | `runtime/src/tool_registry/report.rs:35` | → `SkillFeedback` |
| Boost factor `[0.5, 1.5]` | `runtime/src/tool_registry/report.rs:100` | Same range for skills |
