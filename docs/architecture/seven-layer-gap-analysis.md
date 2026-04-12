# Seven-Layer Resilient Agent Architecture — Gap Analysis

> Produced: 2026-04-12 | Base: `migrate_to_rust` @ `f4fd5d67`

---

## Layer 1: State Layer — Typed & Versioned State Management

### Existing Capabilities
- **CompositeSnapshot** (`core/composite_snapshot.rs`): 5-dimensional snapshot system (Session, DataSnapshot, Memory, GitCommit, Workspace). Each variant carries typed `SnapshotRef` with `serde` tag-based dispatch.
- **SessionStateAccumulator** (`services/session_journal.rs:119`): Tracks session state deltas — tool usage, token consumption, goal events. Strongly typed fields, not raw KV.
- **DataVersioningService** (`services/data_versioning.rs`): Checkpoint CRUD, causal chain queries, trace-upstream lineage. Backed by MatrixOne.
- **DataSnapshotRef**: Carries `snapshot_name`, `databases`, `branch_name` — aligns with Git4Data concept.
- **PatternLibrary** (`pipeline/pattern.rs`): Versioned tool-chain patterns with confidence scores, drift detection.

### Gaps
- **No State Diff**: Cannot compute a typed diff between two snapshots. CompositeSnapshot stores refs, but there's no `diff(snap_a, snap_b) → StateDelta` operation.
- **No State Merge**: No merge semantics for combining divergent state branches (e.g., after parallel agent exploration).
- **No Revert primitive**: Restore-from-snapshot exists at the MatrixOne level, but no unified `revert(snap) → Result` across all 5 dimensions.
- **Schema Enforcement partial**: SessionStateAccumulator is typed, but `edge_profile` uses `HashMap<String, Value>` — raw KV leak.
- **No version counter**: State changes are timestamped but not numbered; no monotonic version for conflict detection.

### Priority Actions
1. Define `StateDiff` trait with `diff()`, `apply()`, `merge()` for each snapshot dimension.
2. Add monotonic `version: u64` to CompositeSnapshot and enforce CAS (compare-and-swap) on writes.

---

## Layer 2: Observation Layer — Evidence Graphs & Structured Metrics

### Existing Capabilities
- **DriftEvidence** (`core/drift.rs`): Typed evidence with `turn`, `evidence_type`, `confidence`. Six evidence types (ToolCallTopicChange, UserCorrection, etc.).
- **CausalChain** (`services/events.rs`, `data_versioning.rs`): `causal_chain_id` on every agent event; `get_causal_chain()` and `trace_upstream()` queries.
- **JournalEvent system** (`services/session_journal.rs`): ~20 event types (tool_call, goal_steered, drift_detected, etc.) with structured payloads.
- **DecisionTrace** (`astra-thin-client/paths.rs:53`): `/chat/session/{id}/decision-trace` API endpoint.
- **StartupTracer** (`astra-cli/startup_trace.rs`): Phase-based startup timing.
- **BridgeHealthMetrics** (`runtime/improvement_proofs`): p99 latency, degradation detection.
- **RoutingMetrics** (`turn/routing_metrics.rs`): ConfidenceCalibrator, DisambiguationAction, precision/recall metrics.

### Gaps
- **No Evidence Graph**: CausalChain is a linear sequence (parent → child). No graph structure for multi-cause or fan-out relationships. Cannot answer "which 3 decisions contributed to this outcome?"
- **No entity-relation evidence links**: DriftEvidence exists for drift only; no general-purpose evidence attachment to arbitrary decisions.
- **Metrics not schema-aligned**: BridgeHealthMetrics are ad-hoc structs; no schema registry linking metrics to State Layer types.
- **No structured metric aggregation pipeline**: Metrics are computed locally; no streaming aggregation or windowed rollups.

### Priority Actions
1. Design `EvidenceGraph` with typed nodes (Decision, Observation, Outcome) and edges (causes, supports, contradicts).
2. Extend `causal_chain_id` to support DAG topology (multiple parents per event).

---

## Layer 3: Action Space — Bounded & Reversible

### Existing Capabilities
- **PermissionManager** (`astra-cli/permission_manager.rs`): Three modes (Prompt/Auto/Deny). Tool-level allow/deny policies.
- **Permission sync** (`runtime/permission_sync_e2e.rs`): Parent-child permission inheritance, mailbox-based request/response.
- **SQL safety validation** (`edge_tools/mo_tools.rs:232`): `check_sql_safety()` blocks DROP/DELETE/TRUNCATE/ALTER/GRANT.
- **Tool allowlist**: Inherited permissions can restrict child agents to specific tool sets.
- **Sandbox checkpointing** (`data_versioning.rs:SandboxCheckpointData`): Checkpoint before risky operations.
- **Action compensation profiles** (`runtime/src/turn/action_compensation.rs`): Typed bounded/reversible/manual rollback metadata now captures action category, pre-state requirements, and compensation kind per tool.
- **Staged mutation contract** (`services/src/mutation_scoreboard.rs`): `MutationCompensationPolicy`, `StagedMutation`, and staged states (`pending/ready/applied/reverted/blocked`) now provide one canonical apply/rollback coordination contract.
- **Mutation lifecycle writeback** (`services/src/session_audit.rs`, `runtime/src/server/audit_handlers.rs`): per-session mutation audits can now record `applied` / `reverted` / `blocked` lifecycle transitions via `mutation_state` events, so staged mutation status is operationally writable instead of read-only.
- **Symlink safety** (`session_checkpoint.rs:97`): Path traversal protection.

### Gaps
- **No automatic compensation executor**: Compensation metadata exists, but no live runner automatically captures pre-state, applies canaries, or executes rollback on failure.
- **Bash is unbounded**: `bash` tool allows arbitrary code execution — directly violates "finite action space" requirement.
- **No action registry**: Tools are discovered dynamically; there's no compile-time exhaustive enumeration of all possible actions.
- **No transactional tool execution**: Multi-tool turns are not atomic — partial failures leave inconsistent state.
- **Rollback execution is still manual**: CompositeSnapshot and staged mutation metadata exist, but there is no automatic "compensation on failure" mechanism yet.

### Priority Actions
1. Define `CompensationAction` trait: every `ToolImpl` optionally provides `compensate(context) → Result`.
2. Implement transaction boundaries for multi-tool turns with automatic rollback on failure.

---

## Layer 4: Evaluation Layer — Multi-Objective, Noise-Robust

### Existing Capabilities
- **EvaluationService** (`services/evaluation/service.rs`): 16-method trait covering quality trends, drift detection, calibration, session scores, SLO dashboards, trust reports, training data export.
- **ConfidenceCalibrator** (`turn/routing_metrics.rs`): Calibrates selection confidence scores.
- **GateValidation** (`services/src/evaluation/database.rs`): Quality gate validation now evaluates recent session-score windows against error-rate and score-regression thresholds, computes score deltas from noise-filtered recent/baseline averages, and persists typed `eval_gate_results` history for later inspection.
- **Quality-trend model filtering** (`services/src/evaluation/database.rs`): `get_quality_trend(model=...)` now filters session-level quality assessments against persisted `agent_events.llm_model_used`, reusing the same "session ever used this model" semantics as the audit surfaces instead of leaving the route as a 501.
- **Noise-filtered quality aggregate** (`services/src/evaluation/database.rs`): `get_quality_trend()` now returns additive `noise_filtered_*` overall metrics using IQR-based outlier rejection on the evaluation window, `run_closed_loop()` prefers that filtered aggregate when deciding whether quality has slipped below the retune threshold, and gate validation reuses the same filter for score-window deltas.
- **Calibration report** (`services/src/evaluation/database.rs`): `get_calibration()` now aggregates latest session-level quality scores against average `context_trace_signal` selection confidence per session, and it also exposes additive noise-filtered calibration summaries by removing session gap outliers before recomputing mean confidence/quality, calibration error, bias, and adjustment guidance.
- **Noise-filtered drift detection** (`services/src/evaluation/database.rs`, `services/src/evaluation/types.rs`): `detect_drift()` now groups raw score samples into current/previous windows per evaluation level, applies the shared IQR filter inside each window, and exposes additive filtered drift averages/deltas. `run_closed_loop()` now bases its drift delta diagnosis on the filtered delta, so single outlier scores are less likely to trigger retune/alert actions.
- **Training-data extraction** (`services/src/evaluation/database.rs`, `services/src/storage.rs`): `extract_training_data()` now snapshots session-level quality examples into persisted `eval_training_datasets` artifacts, so extraction is real even before the export route is implemented.
- **Training-data export** (`services/src/evaluation/database.rs`): persisted training datasets can now be exported as inline JSONL or CSV payloads via the existing export route; only Parquet remains intentionally unsupported.
- **FeedbackSignal system** (`auto_tuning.rs`): 16 signal types (Retry, Correction, Acceptance, StarRating, ToolChurn, FocusDrift, etc.).
- **Noise-filtered token-usage trigger** (`runtime/src/auto_tuning.rs`): `HighTokenUsage` rules now compute their window average from token-bearing samples only, apply an IQR-style outlier filter before comparing against thresholds, and treat `min_samples` as token-sample count instead of counting unrelated feedback chatter. That makes compression/budget adjustments less likely to fire on a single spike plus unrelated signals.
- **Outcome-scoped feedback-rate guards** (`runtime/src/auto_tuning.rs`): `LowSuccessRate` and `HighRetryRate` now compute both their `min_samples` gates and their rate denominators from outcome-bearing samples (`TaskSuccess`, `Acceptance`, `TaskFailure`, `Retry`, `Correction`) instead of the entire feedback window. That keeps quick follow-ups and other chatter from faking enough samples or diluting retry pressure.
- **ScenarioDetector** (`user_profile.rs:520`): Confidence-threshold-based scenario detection.
- **EvolutionRules** (`auto_tuning.rs`): Trigger → action rules with cooldown and rate limiting.
- **Evolution proposal promotion gate** (`runtime/src/evolution/promotion_gate.rs`, `runtime/src/evolution/service.rs`): skill/pattern/calibration proposals now carry typed `ProposalPromotionVerdict` values (`promote`, `canary`, `hold`) with evidence, blockers, and rollback hints. `EvolutionService` only auto-applies `promote`; hard `PatternAction::Block` proposals now fall back to canary/pending instead of silently mutating live behavior on confidence alone.
- **Adaptive-baseline promotion gate** (`runtime/src/adaptive_baselines.rs`, `runtime/src/observability_integration.rs`, `runtime/src/turn/agentic_loop_host.rs`): experiment winners are now scored with the same typed `promote` / `canary` / `hold` discipline before overwriting a live baseline. `ObservabilityHub::promote_experiment_winner()` returns explicit `Promoted` / `Deferred` / `Skipped` outcomes, mixed winners with significant regressions defer instead of silently promoting, and the agentic loop now logs deferred winners instead of making them look like silent `None` no-ops.
- **Runtime evaluation-summary bridge** (`runtime/src/promotion_context.rs`, `runtime/src/server/run_lifecycle.rs`, `runtime/src/turn/agentic_loop_host.rs`): server runs and delegated sub-runs now initialize a live `ObservabilityHub`/session plus `EvolutionService`, preload noise-filtered quality, latest gate, and calibration summaries once at startup, and feed that context into adaptive-baseline and evolution promotion scoring. Strong local proposals can now be deferred or held when the broader service-side judge reports global regressions.
- **Runtime promotion audit views** (`runtime/src/server/run_lifecycle.rs`, `services/src/session_audit.rs`, `runtime/src/server/audit_handlers.rs`, `runtime/src/server/router_builder.rs`): runtime promotion verdicts are now persisted into `agent_events` as typed `runtime_promotion_verdict` records, and the audit surface exposes them at `/sessions/{session_id}/audit/promotions` plus `/audit/promotions`. Ops can now list deferred adaptive-baseline winners and queued/auto-applied evolution proposals without scraping loop logs.
- **Runtime promotion stats** (`services/src/session_audit.rs`): `/audit/stats` now aggregates runtime promotion totals, controller buckets, outcome buckets, and `promote` / `canary` / `hold` recommendation counts directly from persisted `runtime_promotion_verdict` events, so ops can spot deferred/queued/auto-applied pressure without fetching the full promotion list first.
- **MutationScoreboard contract** (`services/src/mutation_scoreboard.rs`): Canonical typed scoreboard now unifies verifier summaries, objective/reward signals, staged mutation state, and aggregated retention metrics.
- **Mutation promotion gate** (`services/src/mutation_scoreboard.rs`, `services/src/session_audit.rs`): staged mutations now carry typed `MutationPromotionVerdict` values (`promote`, `canary`, `hold`) with evidence, blockers, and rollback hints. `Ready` now requires verifier-backed support; strong but verifier-missing mutations fall back to canary/pending, and the cross-session mutation queue can filter/sort directly on the typed recommendation instead of inferring intent only from `state`.
- **Evaluation-summary bridge for mutation promotion** (`services/src/session_audit.rs`): before session and cross-session mutation scoreboards/queues are exposed, the audit service now fetches service-side evaluation summaries (noise-filtered quality, latest gate result, calibration error) and folds them into mutation promotion verdicts. That means a previously-ready mutation can now be downgraded to canary when the broader service-side judge says global quality or gate health is shaky.
- **Decision-audit-backed mutation scoreboard exposure** (`runtime/src/bridge/side_effects.rs`, `services/src/session_audit.rs`, `runtime/src/server/audit_handlers.rs`): tool-selection decision audits now persist `mutation_objective_score`, tool arguments, turn metadata, and available verifier evidence from per-action summaries, verifier-shaped tool results, or conservative same-turn single-action journal fallbacks. When verifier evidence is still absent, the persisted action profile now carries a typed `verifier_gap`, so `/sessions/{session_id}/audit/mutations` exposes both positive verifier signals and explicit missing-signal cases for report/ops inspection.
- **Cross-session mutation stats** (`services/src/session_audit.rs`): `/audit/stats` now aggregates global mutation counts (ready, approval-required, applied, reverted, blocked) plus verifier visibility counters (`verified`, `missing`, `tool_result`, `turn_journal`, and explicit gap buckets) across the user’s sessions, turning the per-session scoreboard into an account-level ops signal.
- **Global mutation queue** (`services/src/session_audit.rs`, `runtime/src/server/audit_handlers.rs`): `/audit/mutations` now lists staged mutations across sessions with priority-first sorting plus state/session/tool/safety/retention filtering, and it can now further isolate `verifier_signal`, `verifier_source`, or `verifier_gap` cases so ops can directly review missing-verifier mutations instead of inferring them from raw payloads.

### Gaps
- **No multi-objective optimization**: Evaluation is single-dimensional (quality score). No Pareto frontier for efficiency × cost × accuracy.
- **Noise filtering is still partial**: quality trend, gate score deltas, calibration summaries, drift windows, token-usage auto-tuning, and outcome-scoped feedback rates now have noise guards, but negative-streak/accumulation triggers and other evaluation metrics are still consumed raw.
- **No fully universal verifier-complete scoreboard yet**: `MutationScoreboard` now persists verifier evidence for fork-skill mutations, generic verifier-shaped tool results, and conservative single-action same-turn journal verification events, and it now explicitly tags missing-signal cases with `verifier_gap`; however, tool paths with no structured or turn-scoped verification signal still lack a real positive verifier surface.
- **Confidence intervals are not universal yet**: sampled evaluation/report surfaces now expose companion `ConfidenceInterval` fields (quality trend, drift, calibration means, session score, gate error rate, trust/SLO, skill success rate, memory confidence), but other unsampled or non-bounded aggregates still lack interval coverage.
- **Parquet export parity is still missing**: `training-data export` now supports JSONL/CSV, but Parquet remains explicitly unsupported on the service side.
- **No verifier diversity**: Gate validation is single-pass, not ensemble-based.

### Priority Actions
1. Keep extending robust filtering beyond quality trends, gate score deltas, calibration, drift, token-usage auto-tuning, and outcome-scoped feedback rates so more evaluation signals participate in the same noise-aware control loop.
2. Extend confidence-interval coverage beyond the current sample-backed evaluation/report surfaces so more summary views expose explicit uncertainty instead of raw point estimates.

---

## Layer 5: Credit Assignment — Diff-Based & Causal

### Existing Capabilities
- **CausalChainId** (`services/events.rs`): Every event carries `causal_chain_id` + `parent_event_id` for lineage.
- **get_causal_chain() / trace_upstream()**: Query lineage forward and backward.
- **goal_steered event** (`session_journal.rs:1861`): Records when agent behavior was steered toward a goal.
- **SessionStateAccumulator**: Tracks per-turn deltas (tokens, tools, goals) — basis for diff computation.
- **PatternLibrary**: Records success/failure counts per tool chain — primitive credit signal.
- **Attribution fields**: `session_id` for attribution on edge tools, durable tasks.

### Gaps
- **No State Diff comparison**: Cannot compute "what changed between turn N and turn N+K" programmatically.
- **No contribution scoring**: No algorithm assigns credit to individual decisions within a causal chain.
- **No causal inference**: Lineage is correlation-based (parent→child); no counterfactual or interventional analysis.
- **No reward signal**: System has no explicit reward/utility function to decompose across decisions.
- **Causal chain is linear**: `parent_event_id` implies single parent — cannot model decision fan-in.

### Priority Actions
1. Implement `StateDiffComputer`: given two snapshots, produce a typed delta with per-field attribution.
2. Add `contribution_score: f64` to `LineageNode` computed from state diff magnitude.

---

## Layer 6: Search Control — Controlled Exploration & Scheduling

### Existing Capabilities
- **SchedulingContract** (`pipeline/step_protocol.rs`): Priority (0-10), timeout, per-tool timeout, retry with exponential backoff.
- **ScenarioRouter** (`scenario_router.rs`): Route queries to specialized profiles with config diffs and metric definitions.
- **Team orchestration** (`server/team_orchestrator.rs`): Budget-based execution, cancellation tokens, fan-out/sequential/adversarial delegation patterns.
- **PatternLibrary diversity**: Records multiple tool chains for similar intents.
- **IntentDisambiguation** (`turn/routing_metrics.rs`): Widens tool selection when ambiguity detected.

### Gaps
- **No proposal diversity constraint**: Planning generates a single execution path; no mechanism to force N diverse candidates.
- **No offline/online decoupling**: All computation is online (request-path). No queue or scheduler for expensive background tasks (training, pattern consolidation, sleep integration).
- **No resource quota system**: Budget is per-execution (token count); no cross-session resource accounting.
- **No exploration/exploitation policy**: Selection is greedy (highest confidence); no UCB/Thompson-sampling for trying new approaches.

### Priority Actions
1. Design `ProposalGenerator` trait that produces N diverse candidate plans with diversity metric.
2. Add background task scheduler for offline pattern consolidation and training data extraction.

---

## Layer 7: Safety Layer — Adversarial Defense

### Existing Capabilities
- **Drift detection** (`core/drift.rs`, `auto_tuning.rs`): DriftCause (5 types), DriftEvidence (6 types), auto-tuning rules for drift-triggered history trimming.
- **Pattern drift alert** (`auto_tuning.rs:1302`): Alert when pattern confidence drops >0.3.
- **SQL safety** (`mo_tools.rs`): Blocks destructive SQL operations.
- **Shared SafetyMiddleware preflight** (`turn/safety_middleware.rs`, `tool_safety_guard.rs`): Centralized request-time guard chain now screens destructive SQL and prompt-injection-style shell obfuscation before live edge tool execution.
- **Learning-boundary causal support gate** (`pipeline/learning.rs`, `turn/contracts.rs`): Trusted-success reinforcement now requires corroborating tool-result/quality evidence, and suspicious high-quality turns are damped instead of blindly reinforcing entity/pattern learning.
- **Permission gating**: Three-tier permission model with inherited restrictions.
- **Stall detection** (`stall.rs`): Detects repeated identical tool calls, empty-name bursts.
- **Symlink safety, path validation**: Guards against path traversal attacks.
- **Evaluation gate** (`evaluation/`): Quality gates that can block deployment.

### Gaps
- **Reward-hacking guard is learning-boundary only**: Suspicious turns stop reinforcing learned success, but live execution still relies on stall/permission controls rather than an active runtime throttle.
- **No model parameter drift detection**: System detects behavioral drift but not weight/embedding drift in fine-tuned models.
- **Anti-hallucinated causality is heuristic-only**: The learning boundary now requires corroborating tool evidence, but it still lacks lineage-backed causal inference and richer cross-turn validation against spurious correlations.
- **No adversarial testing framework**: No red-team/fuzzing infrastructure for systematically probing safety boundaries.
- **Middleware is still thin**: Centralized guards currently cover request-time SQL/shell preflight, but post-tool validation and broader adversarial guard expansion are still scattered.

### Priority Actions
1. Expand centralized `SafetyMiddleware` beyond request-time preflight into broader post-tool/output validation.
2. Extend causal-support heuristics into lineage-backed anti-hallucinated-causality checks plus adversarial testing coverage for learned patterns.

---

## Summary Matrix

| Layer | Maturity | Key Existing Asset | Biggest Gap |
|-------|----------|--------------------|-------------|
| 1. State | ⬛⬛⬛⬜⬜ 60% | CompositeSnapshot 5D | No diff/merge/revert |
| 2. Observation | ⬛⬛⬛⬜⬜ 55% | CausalChain + JournalEvents | Linear chains, no graph |
| 3. Action | ⬛⬛⬜⬜⬜ 45% | Permission 3-tier model + compensation profiles | No automatic rollback executor |
| 4. Evaluation | ⬛⬛⬛⬛⬜ 86% | EvaluationService + MutationScoreboard | Export parity + partial CI/noise coverage |
| 5. Credit | ⬛⬜⬜⬜⬜ 25% | causal_chain_id on events | No diff-based scoring |
| 6. Search | ⬛⬛⬜⬜⬜ 35% | SchedulingContract + TeamOrch | No proposal diversity |
| 7. Safety | ⬛⬛⬛⬜⬜ 55% | Drift detection + middleware | Causal guard is heuristic-only |

## Recommended Build Order

1. **State Layer** (foundation for all others) — StateDiff trait, version counter
2. **Observation Layer** (depends on State) — EvidenceGraph with DAG topology
3. **Evaluation Layer** (depends on Observation) — Implement remaining 501 routes, extend CI coverage
4. **Action Space** (depends on State) — CompensationAction, transaction boundaries
5. **Credit Assignment** (depends on State + Evaluation) — StateDiffComputer, contribution scoring
6. **Safety Layer** (depends on Action + Evaluation) — Expand middleware + deepen causal safeguards
7. **Search Control** (depends on all above) — Proposal diversity, offline scheduler
