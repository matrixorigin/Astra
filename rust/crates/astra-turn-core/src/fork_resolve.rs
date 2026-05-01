//! Spawn-side resolver for [`InheritPrefixSpec`].
//!
//! Given a child's spawn request and a handle to the
//! [`PrefixCaptureSink`], this module answers one question: "can the
//! child safely reuse the captured parent prefix, and if so, which
//! Arc should it attach to its first API request?"
//!
//! Like PR 3's capture helper, this is a **pure function** — no turn
//! state, no runtime types, no I/O. The spawner calls it once at
//! spawn-decision time; the outcome tells the spawner whether to:
//! - attach the prefix (Resolved)
//! - spawn without cache reuse (Fallback)
//! - fail the spawn entirely (Failed)
//!
//! ## Role in the fork-prefix pipeline
//!
//! - PR 1: [`ForkPrefix`] type
//! - PR 2: [`PrefixCaptureSink`] trait + in-memory store
//! - PR 3: capture-site helper + feature flag
//! - **PR 4 (this)**: spawn-side resolver + `SpawnAgentInput`
//!   extensions. Still NOT wired into the live spawner.
//! - PR 4.5: spawner calls the resolver; reconstructor consumes
//!   canonical bytes when assembling the child's first request.
//! - PR 5: `ForkCacheEvent` telemetry on child's first response.
//!
//! ## Why Fallback vs Failed is the caller's choice
//!
//! `InheritPrefixSpec.required` encodes the caller's intent:
//! - `required: false` (default) — prefix reuse is opportunistic.
//!   Missing / incompatible / flag-disabled ⇒ spawn still proceeds
//!   without the prefix. This is the right default for most skills:
//!   cache reuse is a latency/cost win, not a correctness property.
//! - `required: true` — caller specifically depends on the prefix
//!   (e.g. a skill that only makes sense when running against a
//!   specific parent context). Missing ⇒ hard failure so the caller
//!   surfaces the mismatch instead of silently starting a fresh run.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::fork_capture::is_fork_inherit_prefix_enabled;
use crate::fork_prefix::{ForkPrefix, ForkValidationError, ProviderKind, SpawnValidationContext};
use crate::fork_prefix_store::PrefixCaptureSink;
use crate::orchestration_spawn_tool::InheritPrefixSpec;

// ---------------------------------------------------------------------
// Outcome + failure reasons
// ---------------------------------------------------------------------

/// Why a resolve attempt didn't produce an attached prefix.
///
/// The variants name failure **causes** (operational/diagnostic
/// axis) — orthogonal to whether the caller wanted hard-fail vs
/// soft-fallback, which is encoded in the [`PrefixResolveOutcome`]
/// level (Failed vs Fallback wrapping the same `ResolveFailure`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolveFailure {
    /// The fork-inherit-prefix feature flag is off.
    ///
    /// This fires **even when a captured prefix exists** in the
    /// sink — the flag is a kill switch, not a "feature never
    /// used" signal. Telemetry should treat this as an operator
    /// decision to stop inheriting, not as a bug.
    FeatureDisabled,
    /// No prefix captured for `run_id`. Either the parent never
    /// captured (skill-level choice), the capture was Skipped
    /// (microcompact, empty, oversized — see [`crate::fork_capture::SkipReason`]),
    /// or the entry expired / was LRU-evicted.
    NotFound { run_id: String },
    /// A captured prefix was found but validation rejected it. The
    /// embedded [`ForkValidationError`] carries the specific reason
    /// (provider mismatch, model mismatch, thinking budget clamp,
    /// oversized prefix).
    Incompatible {
        run_id: String,
        reason: ForkValidationError,
    },
    /// The inherit spec asked to inherit from the caller's own run,
    /// but no caller run id was provided to the resolver. This is a
    /// wiring bug in the spawner; always surfaced as Failed
    /// regardless of `required`.
    CallerRunIdMissing,
}

/// Structured result of a resolve attempt.
///
/// - `Resolved` — the spawner attaches the `Arc<ForkPrefix>` to the
///   child's request assembly path.
/// - `Fallback` — the spawner proceeds without cache inheritance
///   (child builds a fresh prefix). Telemetry should still record
///   the reason so drift is visible.
/// - `Failed` — the spawner rejects the spawn with an error carrying
///   the reason; the caller (the model) sees a tool error.
/// - `Disabled` — the spawner did not even consider the request
///   because the caller didn't ask for inheritance. Distinct from
///   Fallback so telemetry can tell "nobody asked" from
///   "we tried and failed".
#[derive(Debug, Clone)]
pub enum PrefixResolveOutcome {
    Resolved { prefix: Arc<ForkPrefix> },
    Fallback { reason: ResolveFailure },
    Failed { reason: ResolveFailure },
    Disabled,
}

impl PrefixResolveOutcome {
    /// Convenience: whether the spawner should proceed (either with
    /// or without an attached prefix). Only `Failed` stops the spawn.
    pub fn proceed(&self) -> bool {
        !matches!(self, PrefixResolveOutcome::Failed { .. })
    }

    /// The attached prefix, if any. `None` for every non-Resolved
    /// outcome — spawner should build a fresh prefix in that case.
    pub fn prefix(&self) -> Option<&Arc<ForkPrefix>> {
        match self {
            PrefixResolveOutcome::Resolved { prefix } => Some(prefix),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------
// Resolver context
// ---------------------------------------------------------------------

/// Context the spawner provides to the resolver. Everything here is
/// known at spawn-decision time; no lookups are performed by the
/// resolver beyond the sink `get_prefix` call itself.
#[derive(Debug, Clone)]
pub struct SpawnResolveContext {
    /// The spawner's own run id. Used as the default `from_run_id`
    /// when `InheritPrefixSpec.from_run_id` is `None`.
    pub caller_run_id: Option<String>,
    /// The child's provider (post-resolution, after any
    /// agent_type / config defaulting). Must match the captured
    /// prefix's provider for a cache hit.
    pub child_provider: ProviderKind,
    /// The child's model id (post-resolution).
    pub child_model_id: String,
    /// The child's max_output_tokens, if set. Interacts with the
    /// captured prefix's thinking budget (see
    /// `ForkPrefix::validate_spawn`).
    pub child_max_output_tokens: Option<u32>,
}

// ---------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------

/// Resolve an `InheritPrefixSpec` against a live sink. See the
/// module docstring for the decision matrix.
///
/// Never panics, never I/Os, never logs. The outcome carries every
/// reason the caller needs for telemetry (PR 5).
pub fn resolve_inherit_prefix(
    spec: Option<&InheritPrefixSpec>,
    ctx: &SpawnResolveContext,
    sink: &dyn PrefixCaptureSink,
) -> PrefixResolveOutcome {
    // No inheritance requested — not even an attempt.
    let Some(spec) = spec else {
        return PrefixResolveOutcome::Disabled;
    };

    // Feature flag gate. Even if a prefix was captured when the flag
    // was on, turning it off should stop resolution (mirror the
    // capture-side contract). Caller's `required` still decides
    // hard-fail vs fallback.
    if !is_fork_inherit_prefix_enabled() {
        return dispatch(spec.required, ResolveFailure::FeatureDisabled);
    }

    // Determine which run's prefix to look up. Explicit non-empty
    // `from_run_id` wins; else fall back to caller's own run.
    // Empty strings in either slot are treated as "missing" because
    // they're never a legitimate run id and letting them through
    // would produce a DashMap lookup keyed on "" — a bug magnet.
    let explicit = spec.from_run_id.as_deref().filter(|s| !s.is_empty());
    let caller = ctx.caller_run_id.as_deref().filter(|s| !s.is_empty());
    let target_run_id: String = match (explicit, caller) {
        (Some(id), _) => id.to_string(),
        (None, Some(id)) => id.to_string(),
        // No explicit from_run_id AND no caller run id — wiring
        // bug, not a user error. Always hard-fail regardless of
        // `required`.
        (None, None) => {
            return PrefixResolveOutcome::Failed {
                reason: ResolveFailure::CallerRunIdMissing,
            };
        }
    };

    let Some(prefix) = sink.get_prefix(&target_run_id) else {
        return dispatch(
            spec.required,
            ResolveFailure::NotFound {
                run_id: target_run_id,
            },
        );
    };

    let validation_ctx = SpawnValidationContext {
        child_provider: ctx.child_provider.clone(),
        child_model_id: ctx.child_model_id.clone(),
        child_max_output_tokens: ctx.child_max_output_tokens,
    };

    if let Err(err) = prefix.validate_spawn(&validation_ctx) {
        return dispatch(
            spec.required,
            ResolveFailure::Incompatible {
                run_id: target_run_id,
                reason: err,
            },
        );
    }

    PrefixResolveOutcome::Resolved { prefix }
}

/// Map (required, failure) → Fallback or Failed. Kept as a small
/// helper so all three failure call-sites share exactly one policy.
fn dispatch(required: bool, reason: ResolveFailure) -> PrefixResolveOutcome {
    if required {
        PrefixResolveOutcome::Failed { reason }
    } else {
        PrefixResolveOutcome::Fallback { reason }
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fork_capture::{
        CaptureRequest, FORK_FLAG_TEST_MUTEX, FORK_INHERIT_PREFIX_ENV, ForkCaptureOutcome,
        capture_parent_prefix, restore_fork_flag_raw_for_tests, set_fork_flag_for_tests,
    };
    use crate::fork_prefix::{
        CacheMode, SystemBlock, ThinkingConfigSlice, ToolSchemaEntry, hash_tool_schema,
    };
    use crate::fork_prefix_store::InMemoryPrefixStore;

    // Resolver tests share the crate-global flag mutex with
    // fork_capture tests (defined at the fork_capture definition
    // site). Sharing one lock across modules is what keeps the
    // feature-flag state consistent under `cargo test --lib fork`
    // which runs both modules' tests in parallel.

    struct FlagGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_raw: u8,
    }

    impl FlagGuard {
        fn set(enabled: bool) -> Self {
            let lock = FORK_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_raw = set_fork_flag_for_tests(enabled);
            Self {
                _lock: lock,
                prev_raw,
            }
        }
    }

    impl Drop for FlagGuard {
        fn drop(&mut self) {
            restore_fork_flag_raw_for_tests(self.prev_raw);
        }
    }

    fn wall_now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sample_tool_entry(name: &str) -> ToolSchemaEntry {
        let schema = serde_json::json!({"function": {"name": name}});
        let (bytes, hash) = hash_tool_schema(&schema);
        ToolSchemaEntry {
            name: name.into(),
            canonical_bytes: bytes,
            hash,
        }
    }

    /// Capture a prefix into a fresh store under the given run id.
    /// Returns the populated store. Requires the flag already on.
    fn populated_store(run_id: &str, thinking_budget: Option<u32>) -> InMemoryPrefixStore {
        let store = InMemoryPrefixStore::new();
        let thinking = thinking_budget.map(|b| ThinkingConfigSlice {
            enabled: true,
            budget_tokens: b,
            kind: "enabled".into(),
        });
        let outcome = capture_parent_prefix(
            CaptureRequest {
                parent_run_id: run_id.into(),
                parent_turn_seq: 1,
                provider: ProviderKind::Anthropic,
                model_id: "claude-opus-4-6".into(),
                thinking,
                system_blocks: vec![SystemBlock {
                    bytes: b"sys".to_vec(),
                    has_cache_control: true,
                }],
                tool_schemas: vec![sample_tool_entry("bash")],
                beta_headers: vec![],
                canonical_prefix_bytes: b"canonical".to_vec(),
                cache_mode: CacheMode::Write,
                captured_at_secs: wall_now_secs(),
                microcompact_fired_in_turn: false,
            },
            &store,
        );
        assert!(
            matches!(outcome, ForkCaptureOutcome::Captured { .. }),
            "fixture must capture, got {outcome:?}"
        );
        store
    }

    fn matching_ctx() -> SpawnResolveContext {
        SpawnResolveContext {
            caller_run_id: Some("run-parent".into()),
            child_provider: ProviderKind::Anthropic,
            child_model_id: "claude-opus-4-6".into(),
            child_max_output_tokens: None,
        }
    }

    // --- Disabled path -----------------------------------------------

    #[test]
    fn returns_disabled_when_no_spec_provided() {
        let _g = FlagGuard::set(true);
        let store = InMemoryPrefixStore::new();
        let out = resolve_inherit_prefix(None, &matching_ctx(), &store);
        assert!(matches!(out, PrefixResolveOutcome::Disabled));
        assert!(out.proceed(), "Disabled must let spawn proceed");
        assert!(out.prefix().is_none());
    }

    // --- Feature flag --------------------------------------------------

    #[test]
    fn feature_off_soft_falls_back() {
        let _g = FlagGuard::set(false);
        let store = InMemoryPrefixStore::new(); // empty — doesn't matter
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        match out {
            PrefixResolveOutcome::Fallback {
                reason: ResolveFailure::FeatureDisabled,
            } => {}
            other => panic!("expected Fallback{{FeatureDisabled}}, got {other:?}"),
        }
    }

    #[test]
    fn feature_off_required_hard_fails() {
        let _g = FlagGuard::set(false);
        let store = InMemoryPrefixStore::new();
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: true,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        match out {
            PrefixResolveOutcome::Failed {
                reason: ResolveFailure::FeatureDisabled,
            } => {}
            other => panic!("expected Failed{{FeatureDisabled}}, got {other:?}"),
        }
        assert!(!out.proceed(), "required + missing must stop the spawn");
    }

    // --- NotFound --------------------------------------------------

    #[test]
    fn not_found_soft_falls_back() {
        let _g = FlagGuard::set(true);
        let store = InMemoryPrefixStore::new();
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        match out {
            PrefixResolveOutcome::Fallback {
                reason: ResolveFailure::NotFound { run_id },
            } => {
                assert_eq!(run_id, "run-parent");
            }
            other => panic!("expected Fallback{{NotFound}}, got {other:?}"),
        }
    }

    #[test]
    fn not_found_required_hard_fails() {
        let _g = FlagGuard::set(true);
        let store = InMemoryPrefixStore::new();
        let spec = InheritPrefixSpec {
            from_run_id: Some("run-never-captured".into()),
            required: true,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        assert!(matches!(
            out,
            PrefixResolveOutcome::Failed {
                reason: ResolveFailure::NotFound { .. }
            }
        ));
    }

    // --- Happy path --------------------------------------------------

    #[test]
    fn resolved_returns_arc_from_sink() {
        let _g = FlagGuard::set(true);
        let store = populated_store("run-parent", None);
        let spec = InheritPrefixSpec {
            from_run_id: None, // default to caller_run_id
            required: false,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        let prefix = out.prefix().expect("expected Resolved").clone();
        assert_eq!(prefix.parent_run_id, "run-parent");
        assert!(out.proceed());
    }

    #[test]
    fn resolved_uses_explicit_from_run_id_over_caller() {
        let _g = FlagGuard::set(true);
        let store = populated_store("run-other-parent", None);
        let spec = InheritPrefixSpec {
            from_run_id: Some("run-other-parent".into()),
            required: false,
        };
        // Caller run is "run-parent" but we explicitly ask for the
        // other one — the explicit spec must win.
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        let prefix = out.prefix().expect("expected Resolved").clone();
        assert_eq!(prefix.parent_run_id, "run-other-parent");
    }

    // --- Incompatible validation ------------------------------------

    #[test]
    fn provider_mismatch_soft_falls_back() {
        let _g = FlagGuard::set(true);
        let store = populated_store("run-parent", None);
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let mut ctx = matching_ctx();
        ctx.child_provider = ProviderKind::OpenAi;
        let out = resolve_inherit_prefix(Some(&spec), &ctx, &store);
        match out {
            PrefixResolveOutcome::Fallback {
                reason:
                    ResolveFailure::Incompatible {
                        reason: ForkValidationError::ProviderMismatch { .. },
                        ..
                    },
            } => {}
            other => panic!("expected Fallback{{Incompatible(ProviderMismatch)}}, got {other:?}"),
        }
    }

    #[test]
    fn model_mismatch_required_hard_fails() {
        let _g = FlagGuard::set(true);
        let store = populated_store("run-parent", None);
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: true,
        };
        let mut ctx = matching_ctx();
        ctx.child_model_id = "claude-sonnet-4-6".into();
        let out = resolve_inherit_prefix(Some(&spec), &ctx, &store);
        match out {
            PrefixResolveOutcome::Failed {
                reason:
                    ResolveFailure::Incompatible {
                        reason: ForkValidationError::ModelMismatch { .. },
                        ..
                    },
            } => {}
            other => panic!("expected Failed{{Incompatible(ModelMismatch)}}, got {other:?}"),
        }
    }

    #[test]
    fn thinking_budget_clamp_soft_falls_back() {
        let _g = FlagGuard::set(true);
        let store = populated_store("run-parent", Some(16_000));
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let mut ctx = matching_ctx();
        ctx.child_max_output_tokens = Some(8_000); // would clamp 16k → 8k
        let out = resolve_inherit_prefix(Some(&spec), &ctx, &store);
        match out {
            PrefixResolveOutcome::Fallback {
                reason:
                    ResolveFailure::Incompatible {
                        reason: ForkValidationError::ThinkingBudgetConflict { .. },
                        ..
                    },
            } => {}
            other => {
                panic!("expected Fallback{{Incompatible(ThinkingBudgetConflict)}}, got {other:?}")
            }
        }
    }

    // --- Wiring bugs --------------------------------------------------

    #[test]
    fn empty_caller_run_id_is_treated_as_missing() {
        // Empty string is never a legitimate run id; treating it
        // the same as None prevents a DashMap lookup on "" and
        // surfaces the wiring bug consistently.
        let _g = FlagGuard::set(true);
        let store = InMemoryPrefixStore::new();
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let mut ctx = matching_ctx();
        ctx.caller_run_id = Some(String::new());
        let out = resolve_inherit_prefix(Some(&spec), &ctx, &store);
        assert!(matches!(
            out,
            PrefixResolveOutcome::Failed {
                reason: ResolveFailure::CallerRunIdMissing,
            }
        ));
    }

    #[test]
    fn empty_from_run_id_falls_back_to_caller() {
        // Explicit `from_run_id: Some("")` must not override the
        // caller_run_id — empty string is not a legitimate run id.
        let _g = FlagGuard::set(true);
        let store = populated_store("run-parent", None);
        let spec = InheritPrefixSpec {
            from_run_id: Some(String::new()),
            required: false,
        };
        let out = resolve_inherit_prefix(Some(&spec), &matching_ctx(), &store);
        let prefix = out.prefix().expect("expected Resolved via caller fallback");
        assert_eq!(prefix.parent_run_id, "run-parent");
    }

    #[test]
    fn caller_run_id_missing_always_fails_even_when_not_required() {
        // This is a spawner wiring bug, not a user-facing failure.
        // Hard-fail regardless of `required` so the bug is loud.
        let _g = FlagGuard::set(true);
        let store = InMemoryPrefixStore::new();
        let spec = InheritPrefixSpec {
            from_run_id: None,
            required: false,
        };
        let mut ctx = matching_ctx();
        ctx.caller_run_id = None;
        let out = resolve_inherit_prefix(Some(&spec), &ctx, &store);
        match out {
            PrefixResolveOutcome::Failed {
                reason: ResolveFailure::CallerRunIdMissing,
            } => {}
            other => panic!(
                "expected Failed{{CallerRunIdMissing}} even with required=false, got {other:?}"
            ),
        }
    }

    // --- Outcome conveniences ----------------------------------------

    #[test]
    fn proceed_is_true_for_everything_except_failed() {
        let disabled = PrefixResolveOutcome::Disabled;
        let fallback = PrefixResolveOutcome::Fallback {
            reason: ResolveFailure::NotFound { run_id: "r".into() },
        };
        let failed = PrefixResolveOutcome::Failed {
            reason: ResolveFailure::CallerRunIdMissing,
        };
        assert!(disabled.proceed());
        assert!(fallback.proceed());
        assert!(!failed.proceed());
    }

    #[test]
    fn prefix_accessor_none_for_non_resolved_outcomes() {
        let disabled = PrefixResolveOutcome::Disabled;
        let fallback = PrefixResolveOutcome::Fallback {
            reason: ResolveFailure::NotFound { run_id: "r".into() },
        };
        let failed = PrefixResolveOutcome::Failed {
            reason: ResolveFailure::CallerRunIdMissing,
        };
        assert!(disabled.prefix().is_none());
        assert!(fallback.prefix().is_none());
        assert!(failed.prefix().is_none());
    }

    // --- Tripwire --------------------------------------------------

    #[test]
    fn env_var_name_unchanged() {
        assert_eq!(FORK_INHERIT_PREFIX_ENV, "ASTRA_FORK_INHERIT_PREFIX");
    }
}
