use super::*;
use astra_runtime::observability_integration::TurnTiming;
use astra_runtime::observability_integration::{FuzzyMatchEvent, FuzzyMatchOutcome};
use astra_turn_core::context_assembly_trace::*;
use serde_json::json;

// ─── Helper: create a sample trace ──────────────────────────────────────────

fn sample_trace(turn: &str, total_used: u32, history: u32, pressure: f64) -> ContextAssemblyTrace {
    ContextAssemblyTrace {
        turn_id: turn.to_string(),
        session_id: "test-session".to_string(),
        system_prompt: SystemPromptBreakdown {
            base_persona_tokens: 500,
            environment_tokens: 200,
            user_preferences_tokens: 100,
            context_signals: Default::default(),
            guidance_signals: Default::default(),
            skills_injected: vec![SkillInjection {
                skill_name: "code-review".to_string(),
                skill_version: Some("1.0".to_string()),
                tokens: 300,
                selection_reason: "user requested".to_string(),
            }],
            repository_memories: vec![MemoryInjection {
                memory_id: "mem-1".to_string(),
                memory_type: "semantic".to_string(),
                tokens: 150,
                relevance_score: 0.85,
                content_preview: "Use JWT for auth".to_string(),
            }],
            total_tokens: 1250,
        },
        history: HistorySelectionTrace {
            total_turns_available: 5,
            turns_retained: vec![TurnRetention {
                turn_index: 1,
                role: "user".to_string(),
                tokens: history / 2,
                has_tool_calls: false,
            }],
            turns_compressed: vec![TurnCompression {
                turn_index: 0,
                role: "assistant".to_string(),
                original_tokens: 500,
                compressed_tokens: 200,
                compression_method: CompressionMethod::ToolResultTruncation,
                information_lost: vec!["verbose output".to_string()],
            }],
            turns_dropped: vec![],
            compression_ratio: 0.6,
            tokens_before: 800,
            tokens_after: history,
        },
        memory: MemoryRetrievalTrace {
            query: "how to authenticate users".to_string(),
            candidates_considered: 10,
            memories_selected: vec![MemorySelection {
                memory_id: "mem-1".to_string(),
                memory_type: "semantic".to_string(),
                content_preview: "Use JWT for auth".to_string(),
                relevance_score: 0.85,
                tokens: 50,
                source: MemorySource::Memoria,
            }],
            memories_rejected: vec![MemoryRejection {
                memory_id: "mem-2".to_string(),
                relevance_score: 0.3,
                rejection_reason: RejectionReason::BelowThreshold {
                    threshold: 0.5,
                    score: 0.3,
                },
            }],
            total_tokens: 50,
            retrieval_latency_ms: 15,
        },
        tools: ToolSelectionTrace {
            tools_available: 20,
            tools_selected: vec![ToolSelected {
                tool_name: "bash".to_string(),
                score: 0.95,
                tokens: 100,
                selection_factors: vec![SelectionFactor {
                    factor_name: "intent_match".to_string(),
                    weight: 0.8,
                    contribution: 0.76,
                }],
            }],
            tools_rejected: vec![],
            selection_strategy: "semantic".to_string(),
            selection_confidence: 0.9,
            selection_latency_ms: 5,
        },
        token_budget: TokenBudgetTrace {
            max_tokens: 128000,
            system_prompt_tokens: 1250,
            history_tokens: history,
            memory_tokens: 50,
            tool_schema_tokens: 100,
            user_message_tokens: 200,
            total_used,
            budget_pressure: pressure,
            compression_triggered: pressure > 0.8,
        },
        explanations: vec![],
        ..Default::default()
    }
}

fn make_executor_with_session(
    traces: Vec<ContextAssemblyTrace>,
    timings: Vec<TurnTiming>,
) -> (tempfile::TempDir, ToolExecutor) {
    let dir = tempfile::tempdir().unwrap();
    let mut executor = ToolExecutor::new(dir.path());

    let session =
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session");

    let session_arc = std::sync::Arc::new(std::sync::RwLock::new(session));
    {
        let mut guard = session_arc.write().unwrap();
        guard.context_traces = traces;
        guard.turn_timings = timings;
    }
    executor.observability_session = Some(session_arc);

    (dir, executor)
}

fn make_executor_with_session_and_fuzzy(
    traces: Vec<ContextAssemblyTrace>,
    timings: Vec<TurnTiming>,
    fuzzy_match_events: Vec<FuzzyMatchEvent>,
) -> (tempfile::TempDir, ToolExecutor) {
    let dir = tempfile::tempdir().unwrap();
    let mut executor = ToolExecutor::new(dir.path());

    let session =
        astra_runtime::observability_integration::ObservabilitySession::new_simple("test-session");

    let session_arc = std::sync::Arc::new(std::sync::RwLock::new(session));
    {
        let mut guard = session_arc.write().unwrap();
        guard.context_traces = traces;
        guard.turn_timings = timings;
        guard.fuzzy_match_events = fuzzy_match_events;
    }
    executor.observability_session = Some(session_arc);

    (dir, executor)
}

// ─── No observability session ────────────────────────────────────────────────

#[test]
fn context_analysis_no_session() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path());

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("No observability session")
    );
}

// ─── Empty traces ────────────────────────────────────────────────────────────

#[test]
fn context_analysis_empty_traces() {
    let (_dir, executor) = make_executor_with_session(vec![], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("No context assembly traces")
    );
}

// ─── Turn mode ───────────────────────────────────────────────────────────────

#[test]
fn context_analysis_turn_default() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    // Default mode is "turn", default turn is -1 (latest)
    let result = executor.context_analysis(&json!({}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["turn"], 1);
    assert_eq!(parsed["of_total_turns"], 1);
    assert!(parsed["token_budget"].is_object());
    assert!(parsed["composition"].is_object());
    assert!(parsed["composition"]["system_prompt"]["tokens"].is_number());
    assert!(parsed["composition"]["history"]["tokens"].is_number());
    assert!(parsed["composition"]["memory"]["tokens"].is_number());
    assert!(parsed["composition"]["tool_schemas"]["tokens"].is_number());
    assert!(parsed["composition"]["user_message"]["tokens"].is_number());
}

#[test]
fn context_analysis_turn_explicit() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 2000, 800, 0.6);
    let (_dir, executor) = make_executor_with_session(vec![t1, t2], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn", "turn": 1}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["turn"], 1);
    assert_eq!(parsed["of_total_turns"], 2);
}

#[test]
fn context_analysis_turn_latest() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 2000, 800, 0.6);
    let (_dir, executor) = make_executor_with_session(vec![t1, t2], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn", "turn": -1}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["turn"], 2);
}

#[test]
fn context_analysis_turn_invalid_zero() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn", "turn": 0}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].is_string());
    assert!(parsed["error"].as_str().unwrap().contains("Invalid turn"));
}

#[test]
fn context_analysis_turn_out_of_range() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn", "turn": 5}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].as_str().unwrap().contains("Invalid turn"));
}

#[test]
fn context_analysis_turn_negative_beyond_range() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn", "turn": -5}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].as_str().unwrap().contains("Invalid turn"));
}

// ─── Turn mode: content verification ─────────────────────────────────────────

#[test]
fn context_analysis_turn_has_system_prompt_breakdown() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let sp = &parsed["composition"]["system_prompt"];
    assert_eq!(sp["tokens"], 1250);

    // Should have sub-components
    let subs = sp["sub_components"].as_array().unwrap();
    assert!(subs.len() >= 3); // base_persona, environment, user_preferences

    // Check for skills sub-component
    let skills = subs.iter().find(|c| c["component"] == "skills");
    assert!(skills.is_some(), "should have skills component");
    let skills = skills.unwrap();
    assert!(skills["detail"].is_array());

    // Check for repo_memories sub-component
    let mems = subs
        .iter()
        .find(|c| c["component"] == "repository_memories");
    assert!(mems.is_some(), "should have repository_memories component");
}

#[test]
fn context_analysis_turn_budget_pressure() {
    let trace = sample_trace("T1", 1600, 500, 0.85);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    let budget = &parsed["token_budget"];
    assert_eq!(budget["max_tokens"], 128000);
    assert!(budget["budget_pressure"].as_str().unwrap().contains("85"));
    assert_eq!(budget["compression_triggered"], true);
}

#[test]
fn context_analysis_turn_includes_fuzzy_matching() {
    let trace = sample_trace("T1", 1600, 500, 0.85);
    let fuzzy_events = vec![
        FuzzyMatchEvent {
            turn: 1,
            path: "src/main.rs".to_string(),
            strategy: "line-number-stripped".to_string(),
            outcome: FuzzyMatchOutcome::Matched,
        },
        FuzzyMatchEvent {
            turn: 1,
            path: "src/main.rs".to_string(),
            strategy: "none".to_string(),
            outcome: FuzzyMatchOutcome::NotFound,
        },
    ];
    let (_dir, executor) = make_executor_with_session_and_fuzzy(vec![trace], vec![], fuzzy_events);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["fuzzy_matching"]["events"], 2);
    assert_eq!(parsed["fuzzy_matching"]["matched"], 1);
    assert_eq!(parsed["fuzzy_matching"]["not_found"], 1);
    assert_eq!(
        parsed["fuzzy_matching"]["detail"][0]["strategy"],
        "line-number-stripped"
    );
}

// ─── Turn mode: zero tokens edge case ────────────────────────────────────────

#[test]
fn context_analysis_turn_all_zero_tokens() {
    let mut trace = sample_trace("T1", 0, 0, 0.0);
    trace.token_budget.system_prompt_tokens = 0;
    trace.token_budget.memory_tokens = 0;
    trace.token_budget.tool_schema_tokens = 0;
    trace.token_budget.user_message_tokens = 0;
    trace.system_prompt.total_tokens = 0;
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Should not panic; percentage calculations use .max(1)
    assert!(parsed["turn"].is_number());
    assert!(parsed["composition"].is_object());
}

// ─── Session mode ────────────────────────────────────────────────────────────

#[test]
fn context_analysis_session_single_turn() {
    let trace = sample_trace("T1", 1600, 500, 0.3);
    let timing = TurnTiming {
        turn: 1,
        context_assembly_ms: 10,
        ttft_ms: 200,
        llm_total_ms: 1000,
        tool_execution_ms: 50,
        total_ms: 1260,
    };
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![timing]);

    let result = executor.context_analysis(&json!({"mode": "session"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["session_summary"]["total_turns"], 1);
    assert!(parsed["per_turn"].is_array());
    assert_eq!(parsed["per_turn"].as_array().unwrap().len(), 1);
    assert!(parsed["averages"].is_object());
}

#[test]
fn context_analysis_session_multi_turn() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 3000, 1500, 0.7);
    let t3 = sample_trace("T3", 5000, 3000, 0.95);
    let timings = vec![
        TurnTiming {
            turn: 1,
            context_assembly_ms: 10,
            ttft_ms: 200,
            llm_total_ms: 1000,
            tool_execution_ms: 50,
            total_ms: 1260,
        },
        TurnTiming {
            turn: 2,
            context_assembly_ms: 15,
            ttft_ms: 250,
            llm_total_ms: 1200,
            tool_execution_ms: 80,
            total_ms: 1545,
        },
        TurnTiming {
            turn: 3,
            context_assembly_ms: 20,
            ttft_ms: 300,
            llm_total_ms: 1500,
            tool_execution_ms: 100,
            total_ms: 1920,
        },
    ];
    let (_dir, executor) = make_executor_with_session(vec![t1, t2, t3], timings);

    let result = executor.context_analysis(&json!({"mode": "session"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["session_summary"]["total_turns"], 3);
    assert_eq!(parsed["per_turn"].as_array().unwrap().len(), 3);

    // Trends should exist
    assert!(parsed["trends"]["history_growth_tokens"].is_number());
    assert!(parsed["trends"]["pressure_change"].is_string());

    // Should have compression event from T3 (pressure > 0.8)
    assert!(
        parsed["session_summary"]["compression_events"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn context_analysis_session_no_timings() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 2000, 800, 0.6);
    let (_dir, executor) = make_executor_with_session(vec![t1, t2], vec![]);

    let result = executor.context_analysis(&json!({"mode": "session"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Should still work even without timing data
    assert_eq!(parsed["session_summary"]["total_turns"], 2);
    // Timing defaults to 0
    let first_turn = &parsed["per_turn"][0];
    assert_eq!(first_turn["timing_ms"], 0);
}

#[test]
fn context_analysis_session_includes_fuzzy_matching_summary() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 2000, 800, 0.6);
    let fuzzy_events = vec![
        FuzzyMatchEvent {
            turn: 1,
            path: "src/main.rs".to_string(),
            strategy: "line-number-stripped".to_string(),
            outcome: FuzzyMatchOutcome::Matched,
        },
        FuzzyMatchEvent {
            turn: 1,
            path: "src/main.rs".to_string(),
            strategy: "quote-normalized".to_string(),
            outcome: FuzzyMatchOutcome::Ambiguous,
        },
        FuzzyMatchEvent {
            turn: 2,
            path: "src/lib.rs".to_string(),
            strategy: "exact".to_string(),
            outcome: FuzzyMatchOutcome::Matched,
        },
    ];
    let (_dir, executor) = make_executor_with_session_and_fuzzy(vec![t1, t2], vec![], fuzzy_events);

    let result = executor.context_analysis(&json!({"mode": "session"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["fuzzy_matching"]["events"], 3);
    assert_eq!(parsed["fuzzy_matching"]["matched"], 2);
    assert_eq!(parsed["fuzzy_matching"]["ambiguous"], 1);
    assert_eq!(
        parsed["fuzzy_matching"]["by_strategy"][0]["strategy"],
        "exact"
    );
    assert_eq!(
        parsed["fuzzy_matching"]["by_strategy"][1]["strategy"],
        "line-number-stripped"
    );
}

// ─── Compare mode ────────────────────────────────────────────────────────────

#[test]
fn context_analysis_compare_basic() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 3000, 1500, 0.7);
    let (_dir, executor) = make_executor_with_session(vec![t1, t2], vec![]);

    let result = executor.context_analysis(&json!({"mode": "compare", "turn_a": 1, "turn_b": 2}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["turn_a"], 1);
    assert_eq!(parsed["turn_b"], 2);
    assert!(parsed["total_tokens"]["delta"].is_number());
    assert!(parsed["history"]["delta"].is_number());

    // History should have grown
    let hist_delta = parsed["history"]["delta"].as_i64().unwrap();
    assert_eq!(hist_delta, 1000); // 1500 - 500
}

#[test]
fn context_analysis_compare_defaults() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let t2 = sample_trace("T2", 3000, 1500, 0.7);
    let (_dir, executor) = make_executor_with_session(vec![t1, t2], vec![]);

    // Default: turn_a=1, turn_b=-1
    let result = executor.context_analysis(&json!({"mode": "compare"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert_eq!(parsed["turn_a"], 1);
    assert_eq!(parsed["turn_b"], 2);
}

#[test]
fn context_analysis_compare_same_turn() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let (_dir, executor) = make_executor_with_session(vec![t1], vec![]);

    let result = executor.context_analysis(&json!({"mode": "compare", "turn_a": 1, "turn_b": 1}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Should work — delta is 0 everywhere
    assert_eq!(parsed["turn_a"], 1);
    assert_eq!(parsed["turn_b"], 1);
    assert_eq!(parsed["total_tokens"]["delta"], 0);
}

#[test]
fn context_analysis_compare_invalid_turn() {
    let t1 = sample_trace("T1", 1600, 500, 0.3);
    let (_dir, executor) = make_executor_with_session(vec![t1], vec![]);

    let result = executor.context_analysis(&json!({"mode": "compare", "turn_a": 1, "turn_b": 5}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].as_str().unwrap().contains("Invalid turns"));
}

// ─── Unknown mode ────────────────────────────────────────────────────────────

#[test]
fn context_analysis_unknown_mode() {
    let trace = sample_trace("T1", 1600, 500, 0.5);
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "banana"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    assert!(parsed["error"].as_str().unwrap().contains("Unknown mode"));
}

// ─── pct helper ──────────────────────────────────────────────────────────────

#[test]
fn pct_zero_denominator() {
    // Access the private fn via the module — we test indirectly via JSON output
    let trace = sample_trace("T1", 0, 0, 0.0);
    let mut trace = trace;
    trace.system_prompt.total_tokens = 0;
    let (_dir, executor) = make_executor_with_session(vec![trace], vec![]);

    let result = executor.context_analysis(&json!({"mode": "turn"}));
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

    // Sub-components should have "0.0%" when denominator is 0
    let subs = parsed["composition"]["system_prompt"]["sub_components"]
        .as_array()
        .unwrap();
    for sub in subs {
        let pct = sub["pct_of_system"].as_str().unwrap();
        assert_eq!(pct, "0.0%");
    }
}
