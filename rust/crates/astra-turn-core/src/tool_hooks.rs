//! Pre- and Post-tool hook framework (gap #1).
//!
//! Lets optional extensions observe, modify, or veto tool executions without
//! touching the edge / cloud delivery paths. Hooks are registered once into a
//! [`ToolHookRegistry`] and invoked at three well-defined phases:
//!
//! * [`HookPhase::PreTool`] — before the tool runs. May rewrite the input
//!   arguments or block the call entirely (e.g. permission gates, policy).
//! * [`HookPhase::PostTool`] — after a successful run. May rewrite the
//!   result text (e.g. redaction, compression, attach annotations).
//! * [`HookPhase::PostToolFailure`] — after a failed run. Observation only
//!   in the current API: decisions other than `Continue` are logged and
//!   discarded, because swallowing errors silently would break the tool
//!   contract.
//!
//! Semantics:
//!
//! * Hooks run **in registration order** (stable, Vec-backed registry).
//! * The first `Block` decision short-circuits the remaining pre-hooks. The
//!   tool is never invoked; callers receive [`PreHookOutcome::Blocked`].
//! * `ReplaceInput` / `ReplaceOutput` decisions thread through: each hook
//!   sees the running (possibly-modified) input or output value.
//! * `Continue` is a no-op.
//! * Hooks are async and may await I/O, but authors should keep them fast
//!   since pre-hook latency is on the hot path of every tool call.
//!
//! This module is intentionally self-contained. Wiring into
//! `cloud_tool_delivery`, the edge tool router, etc. is left to follow-up
//! commits so this foundation can land and be reused by unit tests.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The three positions where a hook can observe or intervene in a tool call.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HookPhase {
    /// Before the tool runs. Can rewrite input or block.
    PreTool,
    /// After a successful tool run. Can rewrite the output text.
    PostTool,
    /// After a failed tool run. Currently observation-only.
    PostToolFailure,
}

impl fmt::Display for HookPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookPhase::PreTool => f.write_str("pre_tool"),
            HookPhase::PostTool => f.write_str("post_tool"),
            HookPhase::PostToolFailure => f.write_str("post_tool_failure"),
        }
    }
}

/// Decision a hook returns. Interpretation depends on [`HookPhase`]:
///
/// | phase           | Continue | ReplaceInput | ReplaceOutput | Block |
/// |-----------------|:--------:|:------------:|:-------------:|:-----:|
/// | PreTool         | ok       | applied      | ignored       | stops |
/// | PostTool        | ok       | ignored      | applied       | ignored |
/// | PostToolFailure | ok       | ignored      | ignored       | ignored |
///
/// Ignored decisions are logged internally and otherwise dropped; they do not
/// abort the remaining hook chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookDecision {
    /// Continue the chain with no modification.
    Continue,
    /// Replace the tool input with the supplied JSON value (pre-hooks only).
    ReplaceInput(Value),
    /// Replace the tool output text with the supplied string (post-hooks
    /// only).
    ReplaceOutput(String),
    /// Block the tool call. `reason` surfaces to the caller as an error.
    Block {
        /// Human-readable reason returned to the caller and to the model.
        reason: String,
    },
}

/// Context object threaded through a hook chain. Each call gets its own
/// instance; hooks are free to inspect but not mutate it in place.
#[derive(Clone, Debug)]
pub struct ToolHookContext {
    /// Canonical tool name (matches the registry used by the rest of
    /// `astra_turn_core`).
    pub tool_name: String,
    /// Input JSON as seen by the tool right before this chain runs. During a
    /// pre-hook chain this reflects accumulated `ReplaceInput` decisions.
    pub tool_input: Value,
    /// Output text for post-hook chains. `None` when running pre-hooks or
    /// the failure chain.
    pub tool_output: Option<String>,
    /// Optional opaque correlation id so callers can thread tracing.
    pub call_id: Option<String>,
}

impl ToolHookContext {
    /// Convenience constructor for pre-hook invocation.
    #[must_use]
    pub fn pre(tool_name: impl Into<String>, input: Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_input: input,
            tool_output: None,
            call_id: None,
        }
    }

    /// Convenience constructor for post-hook invocation.
    #[must_use]
    pub fn post(tool_name: impl Into<String>, input: Value, output: String) -> Self {
        Self {
            tool_name: tool_name.into(),
            tool_input: input,
            tool_output: Some(output),
            call_id: None,
        }
    }

    /// Attach a correlation id.
    #[must_use]
    pub fn with_call_id(mut self, id: impl Into<String>) -> Self {
        self.call_id = Some(id.into());
        self
    }
}

/// Trait that a hook implements. Hooks may filter by tool name via
/// [`ToolHook::applies_to`]; the default applies to all tools.
#[async_trait]
pub trait ToolHook: Send + Sync {
    /// Stable id used for `unregister_by_id` and log lines. Should be
    /// globally unique across the registry.
    fn id(&self) -> &str;

    /// Phases this hook wants to run in.
    fn phases(&self) -> &[HookPhase];

    /// Return `false` to skip this hook for the current tool call. Default
    /// applies to every tool.
    #[allow(unused_variables)]
    fn applies_to(&self, tool_name: &str) -> bool {
        true
    }

    /// Hook body. Called once per applicable phase per tool invocation.
    async fn run(&self, phase: HookPhase, ctx: &ToolHookContext) -> HookDecision;
}

/// Outcome of running the pre-hook chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreHookOutcome {
    /// All pre-hooks passed. Input may have been rewritten along the way.
    Proceed {
        /// Final tool input after all `ReplaceInput` decisions.
        final_input: Value,
    },
    /// A pre-hook returned `Block`. Tool should NOT be invoked.
    Blocked {
        /// Id of the hook that blocked.
        hook_id: String,
        /// Human-readable reason surfaced to the caller.
        reason: String,
    },
}

/// Outcome of running the post-hook chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostHookOutcome {
    /// Final tool output after all `ReplaceOutput` decisions.
    pub final_output: String,
    /// Ids of hooks that actually mutated the output, in order. Useful for
    /// observability dashboards.
    pub mutating_hook_ids: Vec<String>,
}

/// Thread-safe Vec-backed registry of hooks.
#[derive(Default)]
pub struct ToolHookRegistry {
    hooks: RwLock<Vec<Arc<dyn ToolHook>>>,
}

impl ToolHookRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            hooks: RwLock::new(Vec::new()),
        }
    }

    /// Register a hook. Hooks are invoked in registration order.
    pub async fn register(&self, hook: Arc<dyn ToolHook>) {
        let mut hooks = self.hooks.write().await;
        hooks.push(hook);
    }

    /// Remove every hook whose id matches. Returns how many were removed.
    pub async fn unregister_by_id(&self, id: &str) -> usize {
        let mut hooks = self.hooks.write().await;
        let before = hooks.len();
        hooks.retain(|h| h.id() != id);
        before - hooks.len()
    }

    /// Snapshot of ids currently registered (in order).
    pub async fn registered_ids(&self) -> Vec<String> {
        let hooks = self.hooks.read().await;
        hooks.iter().map(|h| h.id().to_string()).collect()
    }

    /// Execute the pre-tool chain for the given context. Stops at the first
    /// `Block` decision.
    pub async fn run_pre(&self, ctx: &ToolHookContext) -> PreHookOutcome {
        let hooks = self.snapshot_for_phase(HookPhase::PreTool).await;
        let mut running_input = ctx.tool_input.clone();
        for hook in hooks {
            if !hook.applies_to(&ctx.tool_name) {
                continue;
            }
            let mut working = ctx.clone();
            working.tool_input = running_input.clone();
            match hook.run(HookPhase::PreTool, &working).await {
                HookDecision::Continue => {}
                HookDecision::ReplaceInput(new_input) => running_input = new_input,
                HookDecision::ReplaceOutput(_) => {
                    // Not meaningful in the pre-tool phase; ignore.
                }
                HookDecision::Block { reason } => {
                    return PreHookOutcome::Blocked {
                        hook_id: hook.id().to_string(),
                        reason,
                    };
                }
            }
        }
        PreHookOutcome::Proceed {
            final_input: running_input,
        }
    }

    /// Execute the post-tool (success) chain. Always returns a final output;
    /// `Block` decisions in this phase are ignored (the tool already ran).
    pub async fn run_post(&self, ctx: &ToolHookContext) -> PostHookOutcome {
        let hooks = self.snapshot_for_phase(HookPhase::PostTool).await;
        let mut running_output = ctx.tool_output.clone().unwrap_or_default();
        let mut mutating_hook_ids = Vec::new();
        for hook in hooks {
            if !hook.applies_to(&ctx.tool_name) {
                continue;
            }
            let mut working = ctx.clone();
            working.tool_output = Some(running_output.clone());
            match hook.run(HookPhase::PostTool, &working).await {
                HookDecision::Continue => {}
                HookDecision::ReplaceOutput(new_output) => {
                    running_output = new_output;
                    mutating_hook_ids.push(hook.id().to_string());
                }
                HookDecision::ReplaceInput(_) | HookDecision::Block { .. } => {
                    // Not meaningful post-tool; ignore.
                }
            }
        }
        PostHookOutcome {
            final_output: running_output,
            mutating_hook_ids,
        }
    }

    /// Execute the post-tool-failure chain. Observation only — every
    /// decision except `Continue` is dropped. Returns the set of hook ids
    /// that ran successfully so callers can surface that in logs.
    pub async fn run_post_failure(&self, ctx: &ToolHookContext) -> HashSet<String> {
        let hooks = self.snapshot_for_phase(HookPhase::PostToolFailure).await;
        let mut observed = HashSet::new();
        for hook in hooks {
            if !hook.applies_to(&ctx.tool_name) {
                continue;
            }
            // We still call the hook so it can observe the failure.
            let _ = hook.run(HookPhase::PostToolFailure, ctx).await;
            observed.insert(hook.id().to_string());
        }
        observed
    }

    async fn snapshot_for_phase(&self, phase: HookPhase) -> Vec<Arc<dyn ToolHook>> {
        let hooks = self.hooks.read().await;
        hooks
            .iter()
            .filter(|h| h.phases().contains(&phase))
            .cloned()
            .collect()
    }
}

// ── Global registry bridge ────────────────────────────────────────────────
//
// Integration sites (e.g. `cloud_tool_delivery`, `stream_render`) can call
// [`global_registry`] to obtain a process-wide `ToolHookRegistry`. This keeps
// the wiring surface minimal: the observability hub (or a plugin init path)
// registers hooks once, and every tool dispatch site can consult the same
// registry without threading it through call stacks.

use std::sync::OnceLock;

static GLOBAL_HOOKS: OnceLock<Arc<ToolHookRegistry>> = OnceLock::new();

/// Get (or lazily initialise) the process-wide hook registry.
pub fn global_registry() -> Arc<ToolHookRegistry> {
    GLOBAL_HOOKS
        .get_or_init(|| Arc::new(ToolHookRegistry::new()))
        .clone()
}

/// Register a hook on the global registry.
pub async fn global_register(hook: Arc<dyn ToolHook>) {
    global_registry().register(hook).await;
}

/// Convenience — run the pre-tool chain on the global registry.
pub async fn global_run_pre(ctx: &ToolHookContext) -> PreHookOutcome {
    global_registry().run_pre(ctx).await
}

/// Convenience — run the post-tool (success) chain on the global registry.
pub async fn global_run_post(ctx: &ToolHookContext) -> PostHookOutcome {
    global_registry().run_post(ctx).await
}
/// Returns `true` if any hook is currently registered on the global
/// registry. Tool dispatch sites can use this to skip the
/// `run_pre`/`run_post` call entirely when no hooks exist — a zero-cost
/// fast-path for the common case.
pub async fn global_has_hooks() -> bool {
    !global_registry().registered_ids().await.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Test fixtures ──────────────────────────────────────────────────

    struct CountingHook {
        id: String,
        phases: Vec<HookPhase>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolHook for CountingHook {
        fn id(&self) -> &str {
            &self.id
        }
        fn phases(&self) -> &[HookPhase] {
            &self.phases
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            HookDecision::Continue
        }
    }

    struct ReplaceInputHook {
        id: String,
        new_input: Value,
    }

    #[async_trait]
    impl ToolHook for ReplaceInputHook {
        fn id(&self) -> &str {
            &self.id
        }
        fn phases(&self) -> &[HookPhase] {
            &[HookPhase::PreTool]
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            HookDecision::ReplaceInput(self.new_input.clone())
        }
    }

    struct BlockingHook {
        id: String,
        reason: String,
    }

    #[async_trait]
    impl ToolHook for BlockingHook {
        fn id(&self) -> &str {
            &self.id
        }
        fn phases(&self) -> &[HookPhase] {
            &[HookPhase::PreTool]
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            HookDecision::Block {
                reason: self.reason.clone(),
            }
        }
    }

    struct ReplaceOutputHook {
        id: String,
        out: String,
    }

    #[async_trait]
    impl ToolHook for ReplaceOutputHook {
        fn id(&self) -> &str {
            &self.id
        }
        fn phases(&self) -> &[HookPhase] {
            &[HookPhase::PostTool]
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            HookDecision::ReplaceOutput(self.out.clone())
        }
    }

    struct ScopedHook {
        id: String,
        only_for: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ToolHook for ScopedHook {
        fn id(&self) -> &str {
            &self.id
        }
        fn phases(&self) -> &[HookPhase] {
            &[HookPhase::PreTool]
        }
        fn applies_to(&self, tool_name: &str) -> bool {
            tool_name == self.only_for
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            HookDecision::Continue
        }
    }

    // ── Tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_registry_proceeds_with_original_input() {
        let reg = ToolHookRegistry::new();
        let ctx = ToolHookContext::pre("read_file", json!({"path": "a.txt"}));
        match reg.run_pre(&ctx).await {
            PreHookOutcome::Proceed { final_input } => {
                assert_eq!(final_input, json!({"path": "a.txt"}));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pre_hook_replace_input_threads_through_chain() {
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(ReplaceInputHook {
            id: "rewriter".into(),
            new_input: json!({"path": "b.txt"}),
        }))
        .await;
        let ctx = ToolHookContext::pre("read_file", json!({"path": "a.txt"}));
        match reg.run_pre(&ctx).await {
            PreHookOutcome::Proceed { final_input } => {
                assert_eq!(final_input, json!({"path": "b.txt"}));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pre_hook_block_short_circuits_and_reports_hook_id() {
        let reg = ToolHookRegistry::new();
        let after_calls = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(BlockingHook {
            id: "policy".into(),
            reason: "not allowed".into(),
        }))
        .await;
        reg.register(Arc::new(CountingHook {
            id: "after".into(),
            phases: vec![HookPhase::PreTool],
            calls: after_calls.clone(),
        }))
        .await;
        let ctx = ToolHookContext::pre("bash", json!({"command": "rm -rf /"}));
        match reg.run_pre(&ctx).await {
            PreHookOutcome::Blocked { hook_id, reason } => {
                assert_eq!(hook_id, "policy");
                assert_eq!(reason, "not allowed");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
        assert_eq!(
            after_calls.load(Ordering::SeqCst),
            0,
            "later hook must not run"
        );
    }

    #[tokio::test]
    async fn post_hook_replace_output_is_applied_and_tracked() {
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(ReplaceOutputHook {
            id: "redactor".into(),
            out: "REDACTED".into(),
        }))
        .await;
        let ctx =
            ToolHookContext::post("read_file", json!({"path": "creds"}), "super secret".into());
        let outcome = reg.run_post(&ctx).await;
        assert_eq!(outcome.final_output, "REDACTED");
        assert_eq!(outcome.mutating_hook_ids, vec!["redactor"]);
    }

    #[tokio::test]
    async fn post_hook_chain_applies_rewrites_in_order() {
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(ReplaceOutputHook {
            id: "one".into(),
            out: "first".into(),
        }))
        .await;
        reg.register(Arc::new(ReplaceOutputHook {
            id: "two".into(),
            out: "second".into(),
        }))
        .await;
        let ctx = ToolHookContext::post("t", json!({}), "raw".into());
        let outcome = reg.run_post(&ctx).await;
        assert_eq!(outcome.final_output, "second");
        assert_eq!(outcome.mutating_hook_ids, vec!["one", "two"]);
    }

    #[tokio::test]
    async fn post_hook_ignores_block_decisions() {
        struct BlockerInPost;
        #[async_trait]
        impl ToolHook for BlockerInPost {
            fn id(&self) -> &str {
                "rogue"
            }
            fn phases(&self) -> &[HookPhase] {
                &[HookPhase::PostTool]
            }
            async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
                HookDecision::Block {
                    reason: "too late".into(),
                }
            }
        }
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(BlockerInPost)).await;
        let ctx = ToolHookContext::post("t", json!({}), "hello".into());
        let outcome = reg.run_post(&ctx).await;
        assert_eq!(outcome.final_output, "hello");
        assert!(outcome.mutating_hook_ids.is_empty());
    }

    #[tokio::test]
    async fn applies_to_filter_skips_irrelevant_tools() {
        let reg = ToolHookRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(ScopedHook {
            id: "only_read".into(),
            only_for: "read_file",
            calls: calls.clone(),
        }))
        .await;

        // Irrelevant tool → hook skipped.
        let _ = reg
            .run_pre(&ToolHookContext::pre("write_file", json!({})))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Relevant tool → hook runs.
        let _ = reg
            .run_pre(&ToolHookContext::pre("read_file", json!({})))
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unregister_by_id_removes_all_matching_hooks() {
        let reg = ToolHookRegistry::new();
        let calls = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(CountingHook {
            id: "x".into(),
            phases: vec![HookPhase::PreTool],
            calls: calls.clone(),
        }))
        .await;
        reg.register(Arc::new(CountingHook {
            id: "x".into(),
            phases: vec![HookPhase::PreTool],
            calls: calls.clone(),
        }))
        .await;
        reg.register(Arc::new(CountingHook {
            id: "y".into(),
            phases: vec![HookPhase::PreTool],
            calls: calls.clone(),
        }))
        .await;
        let removed = reg.unregister_by_id("x").await;
        assert_eq!(removed, 2);
        assert_eq!(reg.registered_ids().await, vec!["y".to_string()]);
    }

    #[tokio::test]
    async fn registered_ids_reflects_registration_order() {
        let reg = ToolHookRegistry::new();
        for name in ["a", "b", "c"] {
            reg.register(Arc::new(CountingHook {
                id: name.into(),
                phases: vec![HookPhase::PreTool],
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .await;
        }
        assert_eq!(reg.registered_ids().await, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn phase_filter_only_invokes_subscribed_hooks() {
        let reg = ToolHookRegistry::new();
        let pre_calls = Arc::new(AtomicUsize::new(0));
        let post_calls = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(CountingHook {
            id: "pre_only".into(),
            phases: vec![HookPhase::PreTool],
            calls: pre_calls.clone(),
        }))
        .await;
        reg.register(Arc::new(CountingHook {
            id: "post_only".into(),
            phases: vec![HookPhase::PostTool],
            calls: post_calls.clone(),
        }))
        .await;

        let _ = reg.run_pre(&ToolHookContext::pre("t", json!({}))).await;
        assert_eq!(pre_calls.load(Ordering::SeqCst), 1);
        assert_eq!(post_calls.load(Ordering::SeqCst), 0);

        let _ = reg
            .run_post(&ToolHookContext::post("t", json!({}), String::new()))
            .await;
        assert_eq!(pre_calls.load(Ordering::SeqCst), 1);
        assert_eq!(post_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pre_hook_replace_input_then_block_short_circuits_on_block() {
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(ReplaceInputHook {
            id: "rewriter".into(),
            new_input: json!({"ok": true}),
        }))
        .await;
        reg.register(Arc::new(BlockingHook {
            id: "gate".into(),
            reason: "denied".into(),
        }))
        .await;
        match reg
            .run_pre(&ToolHookContext::pre("t", json!({"ok": false})))
            .await
        {
            PreHookOutcome::Blocked { hook_id, reason } => {
                assert_eq!(hook_id, "gate");
                assert_eq!(reason, "denied");
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn post_failure_chain_observes_without_mutating() {
        struct FailureObserver {
            id: String,
            saw: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl ToolHook for FailureObserver {
            fn id(&self) -> &str {
                &self.id
            }
            fn phases(&self) -> &[HookPhase] {
                &[HookPhase::PostToolFailure]
            }
            async fn run(&self, phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
                assert_eq!(phase, HookPhase::PostToolFailure);
                self.saw.fetch_add(1, Ordering::SeqCst);
                // Even if a hook returns non-Continue, the registry drops it.
                HookDecision::ReplaceOutput("ignored".into())
            }
        }
        let reg = ToolHookRegistry::new();
        let saw = Arc::new(AtomicUsize::new(0));
        reg.register(Arc::new(FailureObserver {
            id: "obs".into(),
            saw: saw.clone(),
        }))
        .await;
        let ctx = ToolHookContext::post("t", json!({}), "err".into());
        let observed = reg.run_post_failure(&ctx).await;
        assert_eq!(saw.load(Ordering::SeqCst), 1);
        assert!(observed.contains("obs"));
    }

    #[tokio::test]
    async fn hook_phase_display_roundtrip() {
        assert_eq!(format!("{}", HookPhase::PreTool), "pre_tool");
        assert_eq!(format!("{}", HookPhase::PostTool), "post_tool");
        assert_eq!(
            format!("{}", HookPhase::PostToolFailure),
            "post_tool_failure"
        );
    }

    #[tokio::test]
    async fn pre_hook_ignores_replace_output_decisions() {
        struct WrongPhase;
        #[async_trait]
        impl ToolHook for WrongPhase {
            fn id(&self) -> &str {
                "wp"
            }
            fn phases(&self) -> &[HookPhase] {
                &[HookPhase::PreTool]
            }
            async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
                HookDecision::ReplaceOutput("should be ignored".into())
            }
        }
        let reg = ToolHookRegistry::new();
        reg.register(Arc::new(WrongPhase)).await;
        match reg.run_pre(&ToolHookContext::pre("t", json!({}))).await {
            PreHookOutcome::Proceed { final_input } => {
                assert_eq!(final_input, json!({}));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_call_id_attaches_correlation() {
        let ctx = ToolHookContext::pre("t", json!({})).with_call_id("abc-123");
        assert_eq!(ctx.call_id.as_deref(), Some("abc-123"));
    }

    // ── Global registry bridge ────────────────────────────────────────────
    //
    // Because the global is process-wide, these tests must not assume
    // isolation from each other. We register hooks with unique ids and
    // only assert that our own ids are present.

    struct TagHook {
        id_: &'static str,
    }

    #[async_trait]
    impl ToolHook for TagHook {
        fn id(&self) -> &str {
            self.id_
        }
        fn phases(&self) -> &[HookPhase] {
            &[
                HookPhase::PreTool,
                HookPhase::PostTool,
                HookPhase::PostToolFailure,
            ]
        }
        async fn run(&self, _phase: HookPhase, _ctx: &ToolHookContext) -> HookDecision {
            HookDecision::Continue
        }
    }

    #[tokio::test]
    async fn global_registry_returns_same_instance() {
        let a = global_registry();
        let b = global_registry();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn global_register_then_global_has_hooks_reports_true() {
        let id = "test-global-register-1-marker";
        global_register(Arc::new(TagHook { id_: id })).await;
        assert!(global_has_hooks().await);
        let ids = global_registry().registered_ids().await;
        assert!(ids.iter().any(|s| s == id));
        // Cleanup so we don't pollute other tests.
        global_registry().unregister_by_id(id).await;
    }

    #[tokio::test]
    async fn global_run_pre_proceeds_when_no_hook_applies() {
        let ctx = ToolHookContext::pre("no-match-tool-xyz-global", json!({"a": 1}));
        match global_run_pre(&ctx).await {
            PreHookOutcome::Proceed { final_input } => {
                assert_eq!(final_input, json!({"a": 1}));
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }
}
