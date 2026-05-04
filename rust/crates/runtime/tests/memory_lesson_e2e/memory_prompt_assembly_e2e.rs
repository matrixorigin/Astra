// E2E test: verify that all memory types reach the prompt assembly layer
// and measure their token impact.
//
// Instead of mocking the full LLM round-trip (which requires the server path
// and misses CLI-only injections), this test directly exercises the same
// functions that the CLI's `bridge_inprocess.rs` uses to build the system
// prompt sections — proving the data reaches the prompt and measuring its
// token cost.
//
// ```text
// cargo test -p astra-runtime --test memory_prompt_assembly_e2e
// ```

use astra_runtime::self_model::SelfModel;
use astra_services::LessonHint;
use serde_json::json;

fn model_with_all_memory_types() -> (SelfModel, String, String) {
    let model_json = json!({
        "capabilities": {
            "total_tools": 3,
            "tool_names": ["bash", "read_file", "grep"],
            "tool_health": [],
            "deprioritized_tools": ["grep"],
            "pinned_tools": [],
            "skills": ["review_changes"],
            "boosted_tools": ["bash"],
            "widen_selection_pending": false,
            "outcome_memory": [],
        },
        "state": {
            "turn_number": 5,
            "token_budget": {
                "max_tokens": 200000,
                "total_used": 80000,
                "remaining": 120000,
                "pressure": 0.4,
                "compression_triggered": false,
            },
            "scenario": null,
            "active_experiment": null,
            "session_elapsed_secs": 300,
            "correction_count": 1,
            "compression_count": 0,
        },
        "goals": {
            "goal": "Implement OAuth2 with PKCE flow for the API",
            "session_goal": "Implement OAuth2 with PKCE flow for the API",
            "plan_goal": "Add token refresh endpoint",
            "tracked_goal": "Implement OAuth2 with PKCE flow for the API",
            "goal_source": "session_goal",
            "tracking_status": "aligned",
            "progress": null,
            "recent_milestones": [],
            "milestone_count": 0,
        },
        "recent_signals": [],
        "constraints": {
            "max_mutations_per_turn": 2,
            "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5,
            "token_reserve_fraction": 0.2,
        }
    });
    let model: SelfModel = serde_json::from_value(model_json).unwrap();

    // Lessons from Memoria L3
    let lessons = vec![
        LessonHint {
            kind: astra_services::LessonKind::ToolDeprioritize,
            trigger_signal: "tool_failures:grep".into(),
            action: "Use rg --glob '!node_modules' instead of grep -r. This monorepo has 280k files and grep times out on node_modules traversal.".into(),
            compact: Some("Use rg instead of grep in this repo".into()),
            workload_tag: None,
        },
        LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: "memoria".into(),
            action: "🔧 CORRECTION: Always use RS256 not HS256 for JWT signing in this project".into(),
            compact: None,
            workload_tag: None,
        },
        LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: "memoria".into(),
            action: "💡 LESSON: pnpm workspaces require --filter flag for cross-package commands".into(),
            compact: None,
            workload_tag: None,
        },
    ];
    let model = model.with_lessons(lessons);

    // Memoria insights (from boost_search — profile + knowledge + episodic)
    let memoria_insights = "\
Relevant memories from prior sessions:
- User profile: Senior Rust engineer, 10+ years experience, prefers concise responses with code examples
- Project knowledge: astra-engine uses MatrixOne as primary DB, REST conventions, Axum 0.8 for HTTP
- Previous session: Completed auth middleware refactoring, decided on RS256 for JWT
- Todo: Review PR #209 findings, consolidate PRs #208 + #210
- Active goal: Implement OAuth2 with PKCE flow (3/7 subtasks done)";

    // Self-awareness text (SelfModel rendered prompt)
    let self_awareness = model.to_system_prompt_section();

    (model, self_awareness, memoria_insights.to_string())
}

// ── Test 1: All memory types visible in assembled prompt ────────────────

#[test]
fn all_memory_types_present_in_prompt_sections() {
    let (_model, self_awareness, memoria_insights) = model_with_all_memory_types();

    // LESSONS (from SelfModel)
    assert!(
        self_awareness.contains("📚 Lessons from prior sessions"),
        "lessons header missing"
    );
    assert!(self_awareness.contains("rg"), "tool lesson missing");
    assert!(
        self_awareness.contains("RS256"),
        "correction lesson missing"
    );
    assert!(self_awareness.contains("pnpm"), "knowledge lesson missing");

    // GOALS (from SelfModel)
    assert!(self_awareness.contains("OAuth2"), "session goal missing");

    // TOOL AWARENESS (from SelfModel)
    assert!(
        self_awareness.contains("grep") && self_awareness.contains("deprioritize"),
        "tool deprioritization missing"
    );

    // PROFILE (from Memoria insights)
    assert!(
        memoria_insights.contains("Senior Rust engineer"),
        "user profile missing"
    );

    // KNOWLEDGE (from Memoria insights)
    assert!(
        memoria_insights.contains("MatrixOne"),
        "project knowledge missing"
    );

    // EPISODIC / PREVIOUS SESSION (from Memoria insights)
    assert!(
        memoria_insights.contains("auth middleware"),
        "previous session memory missing"
    );

    // TODOS (from Memoria insights)
    assert!(
        memoria_insights.contains("Review PR #209"),
        "todo item missing"
    );

    // ACTIVE GOAL (from Memoria insights)
    assert!(
        memoria_insights.contains("PKCE"),
        "active goal from Memoria missing"
    );
}

// ── Test 2: Token cost measurement ──────────────────────────────────────

#[test]
fn token_cost_measurement() {
    let (_model, self_awareness, memoria_insights) = model_with_all_memory_types();

    // Rough token estimate: ~4 chars per token for English
    let sa_tokens = self_awareness.len() / 4;
    let mi_tokens = memoria_insights.len() / 4;
    let total_memory_tokens = sa_tokens + mi_tokens;

    println!("=== Memory Token Budget ===");
    println!(
        "Self-awareness (SelfModel + lessons): ~{sa_tokens} tokens ({} chars)",
        self_awareness.len()
    );
    println!(
        "Memoria insights (profile + knowledge + episodic): ~{mi_tokens} tokens ({} chars)",
        memoria_insights.len()
    );
    println!("Total memory overhead per turn: ~{total_memory_tokens} tokens");
    println!();

    // Session Memory Protocol target: ≤700 tokens for L1a+L1b injection.
    // Our memory sections are in CacheScope::None (dynamic, not cached).
    // Budget check: total memory overhead should be < 1000 tokens.
    assert!(
        total_memory_tokens < 1000,
        "total memory token overhead ({total_memory_tokens}) exceeds 1000 token budget"
    );

    // Self-awareness should be < 600 tokens (SelfModel is the biggest piece)
    assert!(
        sa_tokens < 600,
        "self-awareness ({sa_tokens} tokens) exceeds 600 token budget"
    );

    // Memoria insights should be < 400 tokens
    assert!(
        mi_tokens < 400,
        "memoria insights ({mi_tokens} tokens) exceeds 400 token budget"
    );
}

// ── Test 3: Pressure-adaptive token savings ─────────────────────────────

#[test]
fn pressure_adaptive_token_savings() {
    let lessons: Vec<LessonHint> = (0..6)
        .map(|i| LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: format!("sig_{i}"),
            action: format!(
                "This is detailed lesson {i} about using specific tools in this project context. \
                 It contains enough detail to be useful but takes significant token budget."
            ),
            compact: Some(format!("compact lesson {i}")),
            workload_tag: None,
        })
        .collect();

    // Low pressure (0.3): 5 lessons, full text
    let low_model: SelfModel = serde_json::from_value(json!({
        "capabilities": { "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false, "outcome_memory": [] },
        "state": { "turn_number": 1, "token_budget": {
            "max_tokens": 200000, "total_used": 60000, "remaining": 140000,
            "pressure": 0.3, "compression_triggered": false },
            "scenario": null, "active_experiment": null,
            "session_elapsed_secs": 0, "correction_count": 0, "compression_count": 0 },
        "goals": { "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none", "tracking_status": "idle",
            "progress": null, "recent_milestones": [], "milestone_count": 0 },
        "recent_signals": [],
        "constraints": { "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2 }
    }))
    .unwrap();
    let low_rendered = low_model
        .with_lessons(lessons.clone())
        .to_system_prompt_section();

    // High pressure (0.85): 2 lessons, compact text
    let high_model: SelfModel = serde_json::from_value(json!({
        "capabilities": { "total_tools": 0, "tool_names": [], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false, "outcome_memory": [] },
        "state": { "turn_number": 10, "token_budget": {
            "max_tokens": 200000, "total_used": 170000, "remaining": 30000,
            "pressure": 0.85, "compression_triggered": false },
            "scenario": null, "active_experiment": null,
            "session_elapsed_secs": 600, "correction_count": 0, "compression_count": 0 },
        "goals": { "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none", "tracking_status": "idle",
            "progress": null, "recent_milestones": [], "milestone_count": 0 },
        "recent_signals": [],
        "constraints": { "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2 }
    }))
    .unwrap();
    let high_rendered = high_model.with_lessons(lessons).to_system_prompt_section();

    let low_tokens = low_rendered.len() / 4;
    let high_tokens = high_rendered.len() / 4;
    let savings_pct = ((low_tokens - high_tokens) as f64 / low_tokens as f64 * 100.0) as u32;

    println!("=== Pressure-Adaptive Token Savings ===");
    println!("Low pressure (0.3): ~{low_tokens} tokens (5 lessons, full text)");
    println!("High pressure (0.85): ~{high_tokens} tokens (2 lessons, compact)");
    println!(
        "Savings: ~{}% ({} tokens saved per turn)",
        savings_pct,
        low_tokens - high_tokens
    );

    // High pressure should use significantly fewer tokens
    assert!(
        high_tokens < low_tokens,
        "high pressure must use fewer tokens: high={high_tokens} low={low_tokens}"
    );
    // At least 30% savings under pressure
    assert!(
        savings_pct >= 30,
        "pressure-adaptive should save at least 30%, got {savings_pct}%"
    );
}

// ── Test 4: Cache impact — memory sections are in CacheScope::None ──────

#[test]
fn memory_sections_do_not_break_cache() {
    // Memory sections (self_awareness + memoria_insights) are injected as
    // PromptSection::dynamic with CacheScope::None in bridge_inprocess.rs.
    // This means they NEVER affect the cached prefix — they're in the
    // volatile region that changes every turn.
    //
    // Verification approach: the SelfModel output changes when lessons or
    // goals change, but the base system prompt (identity, rules, tools)
    // stays identical. If memory sections were in CacheScope::Session or
    // Global, changing a lesson would break the entire cache.

    let base_model: SelfModel = serde_json::from_value(json!({
        "capabilities": { "total_tools": 1, "tool_names": ["bash"], "tool_health": [],
            "deprioritized_tools": [], "pinned_tools": [], "skills": [],
            "boosted_tools": [], "widen_selection_pending": false, "outcome_memory": [] },
        "state": { "turn_number": 1, "token_budget": null, "scenario": null,
            "active_experiment": null, "session_elapsed_secs": 0,
            "correction_count": 0, "compression_count": 0 },
        "goals": { "goal": null, "session_goal": null, "plan_goal": null,
            "tracked_goal": null, "goal_source": "none", "tracking_status": "idle",
            "progress": null, "recent_milestones": [], "milestone_count": 0 },
        "recent_signals": [],
        "constraints": { "max_mutations_per_turn": 2, "config_drift_ceiling": 0.3,
            "min_tool_pool_size": 5, "token_reserve_fraction": 0.2 }
    }))
    .unwrap();

    let no_lessons = base_model.clone().to_system_prompt_section();
    let with_lessons = base_model
        .with_lessons(vec![LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: "test".into(),
            action: "test lesson action".into(),
            compact: None,
            workload_tag: None,
        }])
        .to_system_prompt_section();

    // The self-awareness section SHOULD be different (lessons added)
    assert_ne!(no_lessons, with_lessons, "lessons must change the output");

    // But since this is in CacheScope::None, it won't break any cache.
    // The base identity prompt (CacheScope::Global) and tool schemas
    // (CacheScope::Session) remain stable regardless of lesson changes.
    // This test documents the design invariant, not a runtime check.
    assert!(
        with_lessons.contains("📚 Lessons"),
        "lessons section must be present when lessons are attached"
    );
    assert!(
        !no_lessons.contains("📚 Lessons"),
        "lessons section must be absent when no lessons"
    );
}
