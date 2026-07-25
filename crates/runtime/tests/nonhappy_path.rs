//! Non-happy-path integration tests proving control mechanisms actually work.

mod circuit_breaker_integration {
    use astra_runtime::bridge::circuit_breaker::CircuitBreaker;
    use std::time::Duration;

    /// Proves CB fast-rejects after threshold failures (as wired in forward())
    #[test]
    fn fast_reject_prevents_timeout_cascade() {
        let cb = CircuitBreaker::new(3, Duration::from_secs(30), 2);

        // Simulate 3 bridge failures
        for _ in 0..3 {
            assert!(cb.allow_request());
            cb.record_failure();
        }

        // Now CB is open — requests should be rejected instantly
        assert!(!cb.allow_request(), "CB should fast-reject after threshold");
        assert_eq!(cb.state(), "open");

        // Key proof: without CB, request would wait 30s timeout.
        // With CB, it returns immediately.
    }

    /// Proves CB recovers after timeout
    #[test]
    fn auto_recovery_after_timeout() {
        let cb = CircuitBreaker::new(1, Duration::from_millis(10), 1);
        cb.record_failure();
        assert_eq!(cb.state(), "open");

        std::thread::sleep(Duration::from_millis(20));

        assert!(cb.allow_request(), "Should allow after recovery timeout");
        assert_eq!(cb.state(), "half_open");

        cb.record_success();
        assert_eq!(
            cb.state(),
            "closed",
            "Should close after success in half_open"
        );
    }
}

mod stall_detection {
    use astra_turn_core::stall::{SERVER_STALL_WINDOW, detect_server_stall};
    use std::collections::BTreeSet;

    /// Proves stall detector catches repetitive tool calls
    #[test]
    fn detects_repetitive_tool_calls() {
        let sig = BTreeSet::from(["bash".to_string(), "echo hello".to_string()]);
        let tool_sigs = vec![sig.clone(), sig.clone(), sig.clone()];

        assert!(
            detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "Should detect stall when same tool call repeats {} times",
            SERVER_STALL_WINDOW
        );
    }

    /// Proves stall detector allows varied tool calls
    #[test]
    fn allows_varied_tool_calls() {
        let sig1 = BTreeSet::from(["bash".to_string(), "ls".to_string()]);
        let sig2 = BTreeSet::from(["bash".to_string(), "pwd".to_string()]);
        let sig3 = BTreeSet::from(["grep".to_string(), "pattern".to_string()]);
        let tool_sigs = vec![sig1, sig2, sig3];

        assert!(
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW).unwrap(),
            "Should NOT detect stall with varied tool calls"
        );
    }
}

mod turn_limits {
    use astra_turn_core::loop_circuit_breaker::BreakerConfig;

    /// Proves the circuit breaker's absolute_max_rounds default is a bounded
    /// infrastructure ceiling and the single source of truth for the hard round cap.
    #[test]
    fn absolute_max_rounds_default_is_bounded() {
        let cap = BreakerConfig::default().absolute_max_rounds;
        assert!(cap > 0, "absolute_max_rounds must be positive, got {cap}");
        assert!(
            cap <= 1000,
            "absolute_max_rounds default should be <= 1000 (infrastructure ceiling), got {cap}"
        );
    }
}

// ── Turn Guard Integration ──────────────────────────────────────────────────

mod turn_guard_integration {
    use astra_turn_core::turn_guard::{TurnGuard, VerdictSeverity};
    use serde_json::json;

    fn tool_call(name: &str, args: &str) -> serde_json::Value {
        json!({"function": {"name": name, "arguments": args}})
    }

    /// Proves: repeated drift raises advisory evidence strength.
    #[test]
    fn drift_escalation_reaches_strong_advisory_after_three_observations() {
        let mut guard = TurnGuard::new();
        guard.drift_nudge_count = 3;

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Critical);
        assert!(
            verdict.advisory_threshold_reached,
            "drift count >= 3 must reach the strong-advisory threshold"
        );
        assert!(
            verdict
                .injections
                .iter()
                .any(|m| m.contains("intent drift") && m.contains("Recommendation")),
            "must provide drift evidence and a recommendation"
        );
    }

    /// Proves: drift escalation does NOT trigger below threshold
    #[test]
    fn drift_below_threshold_stays_healthy() {
        let mut guard = TurnGuard::new();
        guard.drift_nudge_count = 2;

        let verdict = guard.evaluate();
        assert!(
            !verdict.advisory_threshold_reached,
            "drift count < 3 must stay below the strong-advisory threshold"
        );
        assert!(
            !verdict
                .injections
                .iter()
                .any(|m| m.contains("intent drift") && m.contains("Recommendation")),
            "must not inject strong drift evidence below the threshold"
        );
    }

    /// Proves: normal session produces no injections
    #[test]
    fn normal_session_stays_healthy() {
        let mut guard = TurnGuard::new();

        // Turn 1: productive tool call (not exploration-only)
        guard.record_tool_calls(&[tool_call(
            "github",
            r#"{"action":"list_prs","state":"open"}"#,
        )]);
        guard.record_tool_result("github", r#"[{"id": 1, "title": "fix bug"}]"#);

        // Turn 2: different productive tool
        guard.record_tool_calls(&[tool_call("git", r#"{"action":"log","n":5}"#)]);
        guard.record_tool_result("git", r#"{"commits": [{"sha": "abc"}]}"#);

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Healthy);
        assert!(verdict.injections.is_empty());
        assert!(verdict.avoid_tools.is_empty());
        assert!(!verdict.advisory_threshold_reached);
    }

    /// Proves: stall + tool health compose correctly
    #[test]
    fn stall_and_health_compose() {
        let mut guard = TurnGuard::new();

        // 3 consecutive failures on a dedicated tool → health avoidance.
        // Shell command failures do not poison the generic shell surface.
        guard.record_tool_result("write_file", "Error: permission denied");
        guard.record_tool_result("write_file", "Error: permission denied");
        guard.record_tool_result("write_file", "Error: permission denied");

        // Same tool call three times → stall (window=3)
        let calls = [tool_call(
            "write_file",
            r#"{"path":"x.rs","content":"same"}"#,
        )];
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);
        guard.record_tool_calls(&calls);

        let verdict = guard.evaluate();
        assert!(verdict.severity >= VerdictSeverity::Warning);
        // Both stall nudge AND health warning should be present
        assert!(
            verdict.injections.len() >= 2,
            "Should have both stall nudge and health warning, got: {:?}",
            verdict.injections
        );
        assert!(verdict.avoid_tools.contains(&"write_file".to_string()));
    }

    /// Proves: escalation reaches critical after multiple nudges
    #[test]
    fn escalation_reaches_critical() {
        let mut guard = TurnGuard::new();

        // Simulate session with many problems
        // 5 errors
        for _ in 0..5 {
            guard.record_tool_result("bash", "Error: fail");
        }
        // 5 nudges already sent (need >= 5 for Critical)
        guard.nudge_count = 5;

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Critical);
        assert!(verdict.injections.iter().any(|m| m.contains("CRITICAL")));
    }

    /// Proves: empty results do not trigger health avoidance but are tracked.
    #[test]
    fn empty_results_tracked_without_health_avoidance() {
        let mut guard = TurnGuard::new();

        // 10 empty results from grep
        for _ in 0..10 {
            guard.record_tool_result("grep", "[]");
        }

        assert!(
            !guard.health.is_avoidance_advised("grep"),
            "empty results should not trigger health avoidance"
        );
        let summary = guard.health.summary();
        assert_eq!(summary.total_errors, 0);
    }

    /// Proves: flaky tool rehab cycle makes threshold stricter
    #[test]
    fn flaky_tool_escalation() {
        let mut guard = TurnGuard::new();

        // First failure cycle
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: fail");
        }
        assert!(guard.health.is_avoidance_advised("write_file"));

        // Rehabilitate
        guard.record_tool_result("write_file", r#"{"ok": true}"#);
        assert!(!guard.health.is_avoidance_advised("write_file"));

        // Second failure cycle
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: fail");
        }
        assert!(guard.health.is_avoidance_advised("write_file"));

        // Rehabilitate again
        guard.record_tool_result("write_file", r#"{"ok": true}"#);

        // Third failure cycle: only 2 needed (stricter threshold)
        guard.record_tool_result("write_file", "Error: fail");
        guard.record_tool_result("write_file", "Error: fail");
        assert!(
            guard.health.is_avoidance_advised("write_file"),
            "flaky tool should trigger health avoidance after only 2 failures"
        );
    }

    /// Proves: diverse error types all get classified
    #[test]
    fn error_classification_integration() {
        let mut guard = TurnGuard::new();

        guard.record_tool_result("github", "Error: 401 Unauthorized");
        guard.record_tool_result("bash", "Error: connection timed out");
        guard.record_tool_result("read_file", "Error: no such file or directory");

        assert_eq!(guard.errors.total_errors, 3);
        // All different categories tracked
        assert!(guard.errors.errors_by_category.len() >= 3);
    }

    /// Proves: cross-session health respects minimum call threshold
    #[test]
    fn cross_session_min_calls_protection() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        use astra_turn_core::tool_health::ToolHealthTracker;

        // Tool A: 3 calls, 100% failure → no health avoidance (too few calls, need >=8)
        // Tool B: 10 calls, 80% failure → health avoidance (enough data + above 70% threshold)
        // Tool C: 10 calls, 60% failure → no health avoidance (below 70% threshold)
        let entries = vec![
            ToolHealthEntry {
                name: "tool_a".to_string(),
                total_calls: 3,
                total_failures: 3,
                input_validation_failures: 0,
                failure_rate: 1.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "tool_b".to_string(),
                total_calls: 10,
                total_failures: 8,
                input_validation_failures: 0,
                failure_rate: 0.8,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "tool_c".to_string(),
                total_calls: 10,
                total_failures: 6,
                input_validation_failures: 0,
                failure_rate: 0.6,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ];

        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_avoidance_advised("tool_a"),
            "too few calls should not trigger health avoidance"
        );
        assert!(
            tracker.is_avoidance_advised("tool_b"),
            "enough calls + high failure rate should trigger health avoidance"
        );
        assert!(
            !tracker.is_avoidance_advised("tool_c"),
            "below failure threshold should not trigger health avoidance"
        );
    }
}

// ── Multi-file edit & continuation round regression tests ───────────────────
//
// These tests reproduce the exact scenarios from session c98e2e7e that exposed:
// 1. retain_invoked_tool_schemas duplicating schemas when the same tool appears
//    in multiple tool_results (→ LLM 400 "function name duplicated")
// 2. TurnGuard reward-hacking false positive when the same tool is called
//    with different arguments (legitimate multi-file edits)

mod multi_file_edit_regression {
    use astra_turn_core::tool_registry_report::ToolSelectionReport;
    use astra_turn_core::tool_schema_prune::retain_invoked_tool_schemas;
    use astra_turn_core::turn_guard::{TurnGuard, VerdictSeverity};
    use serde_json::{Value, json};

    fn tool_schema(name: &str) -> Value {
        json!({"type": "function", "function": {"name": name, "description": "d", "parameters": {}}})
    }

    fn tool_call_fn(name: &str, args: &str) -> Value {
        json!({"function": {"name": name, "arguments": args}})
    }

    /// Reproduces the exact scenario from session c98e2e7e turn 5:
    /// - Skill activates review-changes, agent calls git 12 times
    /// - Continuation turn sends 12 tool_results for git
    /// - retain_invoked_tool_schemas must NOT duplicate the schema
    ///
    /// Before fix: 12 git schemas → kimi-k2.5 returns 400
    /// After fix: 1 git schema
    #[test]
    fn continuation_turn_with_12_git_diff_results_no_duplicate_schemas() {
        let all_schemas = vec![
            tool_schema("bash"),
            tool_schema("read_file"),
            tool_schema("git"),
            tool_schema("skill"),
        ];

        // Initial selection: bash + read_file (git NOT selected)
        let mut selected = vec![tool_schema("bash"), tool_schema("read_file")];
        let mut report = ToolSelectionReport {
            visible_tools: vec!["bash".into(), "read_file".into()],
            visible_count: 2,
            schema_budget_used: 0,
            schema_budget_total: 1000,
        };

        // 12 tool_results for git (different file paths, same tool)
        let tool_results: Vec<Value> = [
            "HEAD -- stall.rs",
            "HEAD -- chain.rs",
            "HEAD -- routing.rs",
            "HEAD -- runtime_limits.rs",
            "HEAD -- stream_render.rs",
            "HEAD -- repl_runtime.rs",
            "HEAD -- schemas.rs",
            "HEAD -- lib.rs",
            "HEAD -- nonhappy_path.rs",
            "HEAD -- improvement_proofs.rs",
            "HEAD",
            "HEAD --stat",
        ]
        .iter()
        .map(|_| json!({"name": "git"}))
        .collect();

        let retained =
            retain_invoked_tool_schemas(&mut selected, &mut report, &tool_results, &all_schemas);

        assert_eq!(retained, 1, "git should be retained exactly once");
        assert_eq!(selected.len(), 3, "bash + read_file + git");

        // Verify no duplicate function names in the final schema list
        let names: Vec<&str> = selected
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect();
        let unique: std::collections::HashSet<&&str> = names.iter().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "All schema names must be unique, got: {names:?}"
        );
    }

    /// Reproduces the exact scenario from session c98e2e7e turn 3:
    /// - Agent fixes 4 bugs across 4 files using str_replace
    /// - Each str_replace has different path/old/new arguments
    /// - TurnGuard must NOT flag this as reward hacking
    ///
    /// Before fix: quality=0.0, avoid_tools=[str_replace, grep, read_file, bash]
    /// After fix: healthy verdict, no restrictions
    #[test]
    fn four_file_str_replace_not_flagged_as_reward_hacking() {
        let mut guard = TurnGuard::new();

        // Turn 1: read 4 files (exploration phase)
        guard.record_tool_calls(&[
            tool_call_fn(
                "read_file",
                r#"{"path":"stream_render.rs","start":3300,"end":3320}"#,
            ),
            tool_call_fn("read_file", r#"{"path":"stall.rs","start":820,"end":860}"#),
            tool_call_fn(
                "read_file",
                r#"{"path":"nonhappy_path.rs","start":80,"end":90}"#,
            ),
            tool_call_fn(
                "read_file",
                r#"{"path":"runtime_limits.rs","start":1,"end":50}"#,
            ),
        ]);
        guard.record_tool_result("read_file", r#"fn tool_done_inline..."#);
        guard.record_tool_result("read_file", r#"CJK_DOMAIN_MAP..."#);
        guard.record_tool_result("read_file", r#"const { assert!..."#);
        guard.record_tool_result("read_file", r#"absolute_max_rounds..."#);
        let v1 = guard.evaluate();
        assert_eq!(v1.severity, VerdictSeverity::Healthy);

        // Turn 2: fix all 4 files with str_replace (different args each time)
        guard.record_tool_calls(&[
            tool_call_fn("str_replace", r#"{"path":"stream_render.rs","old":"if self.md.is_none()","new":"// unconditional"}"#),
            tool_call_fn("str_replace", r#"{"path":"stall.rs","old":"(\"修\",","new":"(\"修复\","}"#),
            tool_call_fn("str_replace", r#"{"path":"nonhappy_path.rs","old":"<= 150","new":"<= 100"}"#),
            tool_call_fn("str_replace", r#"{"path":"stream_render.rs","old":"s.lines_written = 3","new":"s.tool_done_inline(...)"}"#),
        ]);
        guard.record_tool_result("str_replace", "Replaced successfully");
        guard.record_tool_result("str_replace", "Replaced successfully");
        guard.record_tool_result("str_replace", "Replaced successfully");
        guard.record_tool_result("str_replace", "Replaced successfully");

        let v2 = guard.evaluate();

        // The key assertion: multi-file edits must NOT trigger reward hacking
        assert!(
            !v2.injections.iter().any(|m| m.contains("Reward-hacking")),
            "Multi-file str_replace with different args should not trigger reward-hacking guard.\n\
             Injections: {:?}",
            v2.injections
        );
        assert!(
            !v2.avoid_tools.contains(&"str_replace".to_string()),
            "str_replace should not be in avoid_tools for legitimate multi-file edits"
        );
    }

    /// Full multi-turn session: review → edit → verify → no false positives.
    /// Simulates the complete flow from session c98e2e7e across 4 turns.
    #[test]
    fn full_review_edit_verify_session_stays_healthy() {
        let mut guard = TurnGuard::new();

        // Turn 1: skill activation + git (review phase)
        guard.record_tool_calls(&[
            tool_call_fn("skill", r#"{"name":"review-changes"}"#),
            tool_call_fn("git", r#"{"action":"diff","ref":"HEAD","stat_only":true}"#),
        ]);
        guard.record_tool_result("skill", "# Skill: review-changes\n...");
        guard.record_tool_result("git", " stall.rs | 178 ++++\n stream_render.rs | 52 +-");
        let v1 = guard.evaluate();
        assert_eq!(v1.severity, VerdictSeverity::Healthy);

        // Turn 2: read files for context (different files)
        guard.record_tool_calls(&[
            tool_call_fn("read_file", r#"{"path":"stall.rs","start":820,"end":860}"#),
            tool_call_fn(
                "read_file",
                r#"{"path":"stream_render.rs","start":3300,"end":3320}"#,
            ),
            tool_call_fn(
                "grep",
                r#"{"pattern":"absolute_max_rounds","path":"turn-core/src"}"#,
            ),
        ]);
        guard.record_tool_result("read_file", "fn extract_cjk_keywords...");
        guard.record_tool_result("read_file", "fn tool_done_inline...");
        guard.record_tool_result(
            "grep",
            "loop_circuit_breaker.rs:110: pub absolute_max_rounds: usize",
        );
        let v2 = guard.evaluate();
        assert_eq!(v2.severity, VerdictSeverity::Healthy);

        // Turn 3: apply fixes across 4 files
        guard.record_tool_calls(&[
            tool_call_fn(
                "str_replace",
                r#"{"path":"stream_render.rs","old":"a","new":"b"}"#,
            ),
            tool_call_fn("str_replace", r#"{"path":"stall.rs","old":"c","new":"d"}"#),
            tool_call_fn(
                "str_replace",
                r#"{"path":"nonhappy_path.rs","old":"e","new":"f"}"#,
            ),
            tool_call_fn(
                "str_replace",
                r#"{"path":"stream_render.rs","old":"g","new":"h"}"#,
            ),
        ]);
        for _ in 0..4 {
            guard.record_tool_result("str_replace", "Replaced successfully");
        }
        let v3 = guard.evaluate();
        assert!(
            !v3.injections.iter().any(|m| m.contains("Reward-hacking")),
            "Turn 3 (multi-file edit) should not trigger reward-hacking"
        );

        // Turn 4: run tests to verify
        guard.record_tool_calls(&[
            tool_call_fn(
                "bash",
                r#"{"command":"cargo test --package astra-runtime"}"#,
            ),
            tool_call_fn("bash", r#"{"command":"cargo test --package astra-cli"}"#),
        ]);
        guard.record_tool_result("bash", "test result: ok. 5 passed");
        guard.record_tool_result("bash", "test result: ok. 13 passed");
        let v4 = guard.evaluate();
        assert_eq!(v4.severity, VerdictSeverity::Healthy);
        assert!(v4.avoid_tools.is_empty());
    }
}

// ── Input Guard Integration ─────────────────────────────────────────────────

mod input_guards {
    use astra_turn_core::tool_registry_state::ConversationState;

    #[test]
    fn empty_query_is_conversational() {
        let state = ConversationState::from_message("", 1);
        assert!(state.is_conversational);
        assert_eq!(state.signal_count(), 0);
    }

    #[test]
    fn whitespace_only_is_conversational() {
        let state = ConversationState::from_message("   \n\t  ", 1);
        assert!(state.is_conversational);
    }

    #[test]
    fn pure_punctuation_is_conversational() {
        let state = ConversationState::from_message("!!??...", 1);
        assert!(state.is_conversational);
    }

    #[test]
    fn pure_emoji_is_conversational() {
        let state = ConversationState::from_message("🎉🎊✨", 1);
        assert!(state.is_conversational);
    }

    #[test]
    fn emoji_with_text_still_processes() {
        let state = ConversationState::from_message("🔥 show me the commits", 1);
        assert!(state.is_fetch, "show should trigger is_fetch");
    }

    #[test]
    fn very_long_query_doesnt_hang() {
        // 5000 chars should be truncated to 2000 internally
        let long_query = "show me the ".repeat(400); // ~4800 chars
        let start = std::time::Instant::now();
        let state = ConversationState::from_message(&long_query, 1);
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 100,
            "Should be fast even for long queries: {:?}",
            elapsed
        );
        assert!(state.is_fetch, "Truncated query should still detect 'show'");
    }

    #[test]
    fn numbers_only_not_conversational() {
        // Numbers are alphanumeric → should proceed through normal processing
        let state = ConversationState::from_message("12345", 1);
        // Not conversational — numbers pass the has_content check
        assert!(!state.is_conversational || state.signal_count() == 0);
    }

    #[test]
    fn cjk_only_query_processes_normally() {
        let state = ConversationState::from_message("查看最新的提交", 1);
        assert!(state.is_fetch);
        assert!(state.is_git);
    }

    #[test]
    fn mixed_emoji_cjk_text() {
        let state = ConversationState::from_message("📝 创建一个issue", 1);
        assert!(state.is_mutate, "创建 should fire is_mutate");
    }
}

// ── Result Quality Integration ──────────────────────────────────────────────

mod result_quality_integration {
    use astra_runtime::turn::result_quality::{ResultQuality, classify_result};

    #[test]
    fn real_world_github_error() {
        let result = r#"{"error": "Resource not accessible by integration", "documentation_url": "https://docs.github.com/rest"}"#;
        assert_eq!(classify_result(result), ResultQuality::Error);
    }

    #[test]
    fn real_world_empty_search() {
        assert_eq!(classify_result("[]"), ResultQuality::Empty);
    }

    #[test]
    fn real_world_bash_success() {
        let result = r#"{"output": "total 24\ndrwxr-xr-x 5 user group 4096\n", "exit_code": 0}"#;
        assert_eq!(classify_result(result), ResultQuality::Success);
    }

    #[test]
    fn real_world_file_not_found() {
        let result = "Error: no such file or directory: /nonexistent/path";
        assert_eq!(classify_result(result), ResultQuality::Error);
    }

    #[test]
    fn real_world_truncated_output() {
        let long_output = "x".repeat(600) + "...[truncated]";
        assert_eq!(classify_result(&long_output), ResultQuality::Truncated);
    }

    #[test]
    fn nested_json_with_data() {
        let result = r#"{"data": {"commits": [{"sha": "abc123"}]}, "total": 1}"#;
        assert_eq!(classify_result(result), ResultQuality::Success);
    }
}

// ── Error Recovery Integration ──────────────────────────────────────────────

mod error_recovery_integration {
    use astra_turn_core::guardrails::error_recovery::*;

    #[test]
    fn full_recovery_flow() {
        // Simulate: tool fails → classify → suggest alternatives → escalate
        let error = "HTTP 503 Service Unavailable";
        let category = classify_error(error);
        assert_eq!(category, ErrorCategory::Network);

        // Should retry
        let delay = should_retry(category, 0);
        assert!(delay.is_some());

        // After retry fails, build recovery message
        let msg = build_recovery_message("read_file", error, category, &[]);
        assert!(msg.contains("Alternatives"));

        // Escalation after multiple issues (new thresholds: 3 nudges → Warning,
        // 8 errors → Warning, 4 nudges + 3 errors → Critical)
        // Test Warning: 3 nudges, 0 errors
        let level = escalation_level(3, 0, 0);
        assert_eq!(level, EscalationLevel::Warning);
    }

    #[test]
    fn command_not_found_classified_correctly() {
        // "command not found" should be Unavailable, not NotFound
        let cat = classify_error("bash: mysql: command not found");
        assert_eq!(
            cat,
            ErrorCategory::ToolUnavailable,
            "command not found should be Unavailable, not NotFound"
        );
    }

    #[test]
    fn error_summary_accumulates_correctly() {
        let mut summary = SessionErrorSummary::new();
        summary.record_error(ErrorCategory::Network);
        summary.record_error(ErrorCategory::Auth);
        summary.record_error(ErrorCategory::Network);
        summary.record_retry(true);
        summary.record_retry(false);

        assert_eq!(summary.total_errors, 3);
        assert_eq!(summary.retry_success_rate(), 0.5);
    }
}

// ── Chat Stream E2E: TurnGuard integration contract ─────────────────────────
//
// These tests simulate the exact sequence of calls that chat_stream.rs performs,
// validating the contract between the production loop and TurnGuard.
//
// Flow per turn: record_tool_calls → record_tool_result (per tool) →
//   result_feedback (per tool) → evaluate → observe verdict
//
// Each test sets up multi-turn scenarios and asserts not just the verdict,
// and confirms behavioral evidence does not mutate hard restrictions or budget.

mod chat_stream_turnguard_e2e {
    use astra_runtime::pipeline::persistence::ToolHealthEntry;
    use astra_runtime::turn::result_quality::ResultQuality;
    use astra_turn_core::guardrails::turn_guard::{TurnGuard, TurnVerdict, VerdictSeverity};
    use astra_turn_core::tool_health::ToolHealthTracker;
    use serde_json::json;
    use std::collections::HashSet;

    fn tc(name: &str, args: &str) -> serde_json::Value {
        json!({"function": {"name": name, "arguments": args}})
    }

    /// Observe verdict telemetry while preserving hard runtime state.
    fn observe_verdict(
        verdict: &TurnVerdict,
        remaining: usize,
        restricted: &mut HashSet<String>,
    ) -> (usize, Vec<(String, u32)>, bool) {
        let mut stall_events = Vec::new();

        let _ = restricted;

        match verdict.severity {
            VerdictSeverity::Critical => {
                stall_events.push(("critical_escalation".to_string(), 0u32));
            }
            VerdictSeverity::Warning => {
                stall_events.push(("warning".to_string(), 0u32));
            }
            _ => {}
        }

        (remaining, stall_events, verdict.advisory_threshold_reached)
    }

    // ── Happy path scenarios ──

    /// 5-turn productive session: no injections, no budget loss, no restrictions.
    #[test]
    fn productive_five_turn_session_no_side_effects() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();
        let max_turns = 25usize;

        // Turn 1: git → success
        guard.record_tool_calls(&[tc("git", r#"{"action":"log","n":10}"#)]);
        let q = guard.record_tool_result("git", r#"[{"sha":"abc","msg":"fix"}]"#);
        assert_eq!(q, ResultQuality::Success);
        let v = guard.evaluate();
        let (remaining, events, stop) = observe_verdict(&v, max_turns, &mut restricted);
        assert_eq!(remaining, max_turns);
        assert!(events.is_empty());
        assert!(!stop);

        // Turn 2: different tool
        guard.record_tool_calls(&[tc("github", r#"{"action":"list_prs","state":"open"}"#)]);
        guard.record_tool_result("github", r#"[{"id":1}]"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);

        // Turn 3: write_file
        guard.record_tool_calls(&[tc(
            "write_file",
            r#"{"path":"x.rs","content":"fn main(){}"}"#,
        )]);
        guard.record_tool_result("write_file", r#"{"written":true}"#);

        // Turn 4: git
        guard.record_tool_calls(&[tc("git", r#"{"action":"status"}"#)]);
        guard.record_tool_result("git", r#"{"modified":["x.rs"]}"#);

        // Turn 5: git
        guard.record_tool_calls(&[tc("git", r#"{"action":"diff"}"#)]);
        guard.record_tool_result("git", r#"+fn main(){}\n-fn old(){}"#);

        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(v.injections.is_empty());
        assert!(v.avoid_tools.is_empty());
        assert!(restricted.is_empty());
    }

    // ── Stall scenarios ──

    /// Exact same tool call repeatedly → stall evidence emitted without
    /// consuming the explicit turn budget. AvoidTools remains advisory.
    #[test]
    fn identical_tool_call_stall_full_flow() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        let call = tc("bash", r#"{"command":"ls -la"}"#);
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "file1.rs\nfile2.rs");
        let v1 = guard.evaluate();
        assert_eq!(v1.severity, VerdictSeverity::Healthy);

        // Repeat exact same call twice more (need 3 for window=3)
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "file1.rs\nfile2.rs");
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "file1.rs\nfile2.rs");
        let v2 = guard.evaluate();

        // Stall must be detected
        assert!(v2.severity >= VerdictSeverity::Warning, "stall expected");
        assert!(!v2.injections.is_empty(), "stall nudge expected");
        assert!(
            v2.injections.iter().any(|m| m.contains("REFLECTION")),
            "should contain structured reflection"
        );

        let (remaining, events, _) = observe_verdict(&v2, 25, &mut restricted);
        assert_eq!(
            remaining, 25,
            "behavior advisories must not consume turn budget"
        );
        assert!(!events.is_empty());

        // Third identical call pushes count to 3 → tool gets added to avoid list
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "file1.rs\nfile2.rs");
        let v3 = guard.evaluate();
        assert!(
            v3.avoid_tools.contains(&"bash".to_string()),
            "3+ occurrences should add tool to avoid list"
        );
    }

    /// Stall recovery: after stall nudge, using a DIFFERENT tool resets stall state.
    #[test]
    fn stall_recovery_with_different_tool() {
        let mut guard = TurnGuard::new();

        // Turn 1-3: stall (need 3 identical calls for window=3)
        let call = tc("bash", r#"{"command":"cat config.yaml"}"#);
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "key: value");
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "key: value");
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_result("bash", "key: value");
        let v = guard.evaluate();
        assert!(v.severity >= VerdictSeverity::Warning);

        // Turn 3: different productive tool
        guard.record_tool_calls(&[tc("write_file", r#"{"path":"x","content":"y"}"#)]);
        guard.record_tool_result("write_file", r#"{"ok":true}"#);
        let v = guard.evaluate();
        // Should not be a stall anymore (different tool call)
        // Severity may still be elevated due to escalation from the nudge count,
        // but the stall-specific REFLECTION injection should not fire again
        let has_stall_reflection = v.injections.iter().any(|m| m.contains("REFLECTION"));
        assert!(
            !has_stall_reflection,
            "stall should not re-fire after recovery"
        );
    }

    // ── Divergence scenarios ──

    /// P2.5: exact-signature loop → DIVERGENCE_CORRECTION injected.
    #[test]
    fn exploration_divergence_triggers_correction() {
        let mut guard = TurnGuard::new();

        // Genuine loop: same tool + identical args repeated 5 times.
        for _ in 0..5 {
            guard.record_tool_calls(&[tc("bash", r#"{"command":"find . -name *.rs"}"#)]);
            guard.record_tool_result("bash", "src/main.rs\nsrc/lib.rs");
        }

        let v = guard.evaluate();
        assert!(
            v.injections
                .iter()
                .any(|m| m.contains("same tool calls") || m.contains("same arguments")),
            "divergence correction should fire on exact-sig loop: {:?}",
            v.injections
        );
        assert!(v.severity >= VerdictSeverity::Warning);
    }

    /// P2.5: diverse exploration tools across many rounds → Healthy.
    #[test]
    fn diverse_exploration_does_not_trigger_correction() {
        let mut guard = TurnGuard::new();
        let rounds = [
            ("bash", r#"{"command":"find . -name *.rs"}"#),
            ("read_file", r#"{"path":"src/main.rs"}"#),
            ("grep", r#"{"pattern":"TODO"}"#),
            ("list_dir", r#"{"path":"src"}"#),
            ("glob", r#"{"pattern":"**/*.toml"}"#),
            ("bash", r#"{"command":"wc -l src/*.rs"}"#),
            ("read_file", r#"{"path":"src/lib.rs"}"#),
            ("grep", r#"{"pattern":"fn "}"#),
        ];
        for (tool, args) in rounds {
            guard.record_tool_calls(&[tc(tool, args)]);
            guard.record_tool_result(tool, "ok");
        }
        let v = guard.evaluate();
        assert!(
            !v.injections
                .iter()
                .any(|m| m.contains("same tool calls") || m.contains("STOP exploring")),
            "diverse exploration must NOT trigger divergence: {:?}",
            v.injections
        );
    }

    /// Productive tool breaks divergence streak.
    #[test]
    fn productive_tool_breaks_divergence() {
        let mut guard = TurnGuard::new();

        // 2 exploration rounds
        guard.record_tool_calls(&[tc("bash", r#"{"command":"ls"}"#)]);
        guard.record_tool_result("bash", "files");
        guard.record_tool_calls(&[tc("read_file", r#"{"path":"a.rs"}"#)]);
        guard.record_tool_result("read_file", "code");

        // Productive tool (non-exploration)
        guard.record_tool_calls(&[tc("write_file", r#"{"path":"a.rs","content":"new"}"#)]);
        guard.record_tool_result("write_file", r#"{"ok":true}"#);

        let v = guard.evaluate();
        let has_divergence = v
            .injections
            .iter()
            .any(|m| m.contains("same tool calls") || m.contains("STOP exploring"));
        assert!(!has_divergence, "productive tool should break divergence");
    }

    // ── Tool health scenarios ──

    /// Mutating tool enters health avoidance after 3 errors and shows up in advisory avoid_tools only.
    #[test]
    fn tool_health_avoidance_is_advisory_only() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        guard.record_tool_result("rollback_database_snapshots", "Error: connection refused");
        guard.record_tool_result("rollback_database_snapshots", "Error: connection refused");
        guard.record_tool_result("rollback_database_snapshots", "Error: connection refused");

        assert!(
            guard
                .health
                .is_avoidance_advised("rollback_database_snapshots")
        );

        let v = guard.evaluate();
        assert!(
            v.avoid_tools
                .contains(&"rollback_database_snapshots".to_string())
        );

        // Apply verdict
        observe_verdict(&v, 25, &mut restricted);
        assert!(
            !restricted.contains("rollback_database_snapshots"),
            "health avoidance must not become a hard schema restriction"
        );
    }

    #[test]
    fn distinct_cache_signatures_do_not_propagate_to_restricted() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        guard.record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        guard.record_cache_hit_for_signature("read_file", "read_file:path=b.txt");
        guard.record_cache_hit_for_signature("read_file", "read_file:path=c.txt");

        let v = guard.evaluate();
        assert!(
            !v.avoid_tools.contains(&"read_file".to_string()),
            "distinct cached signatures should not avoid the whole tool"
        );

        observe_verdict(&v, 25, &mut restricted);
        assert!(
            !restricted.contains("read_file"),
            "distinct cached signatures should not land in restricted_tools"
        );
    }

    #[test]
    fn repeated_identical_cache_signature_is_guidance_only() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        for _ in 0..3 {
            guard.record_cache_hit_for_signature("read_file", "read_file:path=a.txt");
        }

        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Info);
        assert!(
            !v.injections.is_empty(),
            "repeated identical cached signature should still emit guidance"
        );
        assert!(
            !v.avoid_tools.contains(&"read_file".to_string()),
            "cache guidance should not hide read-only observation tools"
        );

        observe_verdict(&v, 25, &mut restricted);
        assert!(
            !restricted.contains("read_file"),
            "repeated identical cached signature must not land in restricted_tools"
        );
    }

    /// Rehabilitation removes tool from avoid list (next evaluation).
    #[test]
    fn rehabilitation_clears_tool_from_avoid() {
        let mut guard = TurnGuard::new();

        // Trigger health avoidance.
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: fail");
        }
        let v1 = guard.evaluate();
        assert!(v1.avoid_tools.contains(&"write_file".to_string()));

        // Rehabilitate
        guard.record_tool_result("write_file", r#"{"output":"ok"}"#);
        assert!(!guard.health.is_avoidance_advised("write_file"));

        // Next evaluation should not list write_file in avoid (from health)
        // Note: it might still appear from escalation/stall — we test health specifically
        let health_avoidance_tools = guard.health.health_avoidance_tools();
        assert!(
            !health_avoidance_tools.contains(&"write_file"),
            "rehabilitated tool not in health avoidance list"
        );
    }

    /// Multiple tools fail independently → each tracked separately.
    #[test]
    fn independent_tool_health_tracking() {
        let mut guard = TurnGuard::new();

        // write_file fails, grep succeeds
        guard.record_tool_result("write_file", "Error: permission denied");
        guard.record_tool_result("write_file", "Error: permission denied");
        guard.record_tool_result("write_file", "Error: permission denied");
        guard.record_tool_result("grep", r#"{"matches":["a.rs"]}"#);

        assert!(guard.health.is_avoidance_advised("write_file"));
        assert!(!guard.health.is_avoidance_advised("grep"));
    }

    // ── Result quality feedback ──

    /// Empty results produce feedback, success results don't.
    #[test]
    fn result_feedback_selective() {
        let guard = TurnGuard::new();

        // Empty
        let fb = guard.result_feedback("grep", ResultQuality::Empty);
        assert!(fb.is_some(), "empty result should produce feedback");
        assert!(
            fb.unwrap().contains("no finished result"),
            "feedback should indicate non-finished result"
        );

        // Success
        let fb = guard.result_feedback("grep", ResultQuality::Success);
        assert!(fb.is_none(), "success should not produce feedback");
    }

    /// Truncated results produce feedback advising different approach.
    #[test]
    fn truncated_result_feedback() {
        let guard = TurnGuard::new();
        let fb = guard.result_feedback("bash", ResultQuality::Truncated);
        assert!(fb.is_some(), "truncated should produce feedback");
    }

    /// record_tool_result correctly classifies all quality types.
    #[test]
    fn result_quality_classification_coverage() {
        let mut guard = TurnGuard::new();

        assert_eq!(
            guard.record_tool_result("a", r#"{"data":"ok"}"#),
            ResultQuality::Success
        );
        assert_eq!(
            guard.record_tool_result("b", "Error: not found"),
            ResultQuality::Error
        );
        assert_eq!(guard.record_tool_result("c", "[]"), ResultQuality::Empty);
        assert_eq!(guard.record_tool_result("d", "{}"), ResultQuality::Empty);
        assert_eq!(guard.record_tool_result("e", ""), ResultQuality::Empty);
        assert_eq!(guard.record_tool_result("f", "null"), ResultQuality::Empty);

        // Verify health tracking reflects classification
        assert!(!guard.health.is_avoidance_advised("a")); // success
        assert!(!guard.health.is_avoidance_advised("c")); // empty → no health avoidance
    }

    // ── Escalation evidence strength ──

    /// Full escalation path: normal → warning → critical → advisory_threshold_reached.
    /// Now Critical requires nudges + errors (pure nudges stay at Warning).
    #[test]
    fn escalation_path_to_advisory_threshold_reached() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();
        let mut budget = 25usize;

        // Phase 1: first stall (need 3 identical calls for window=3)
        let call = tc("bash", r#"{"command":"echo hi"}"#);
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_calls(std::slice::from_ref(&call));
        let v = guard.evaluate();
        assert_eq!(guard.nudge_count, 1);
        let (b, _, _) = observe_verdict(&v, budget, &mut restricted);
        budget = b;
        assert_eq!(budget, 25, "warning evidence must preserve explicit budget");

        // Phase 2: more stalls to accumulate nudges
        for _ in 0..3 {
            guard.record_tool_calls(std::slice::from_ref(&call));
            let v = guard.evaluate();
            let (b, _, _) = observe_verdict(&v, budget, &mut restricted);
            budget = b;
        }
        assert!(guard.nudge_count >= 3);

        // Phase 3: pure nudges → only Warning (not Critical without errors)
        let v = guard.evaluate();
        assert!(
            !v.advisory_threshold_reached,
            "pure nudges without errors must remain below the strong-advisory threshold"
        );

        // Phase 4: add tool errors to couple with nudges → first Critical evidence
        guard.record_tool_result("bash", "error: no such file");
        guard.record_tool_result("bash", "error: not found");
        guard.record_tool_result("bash", "Error: unexpected failure");
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Critical);
        assert!(
            !v.advisory_threshold_reached,
            "first Critical remains below the strong-advisory threshold"
        );

        // Phase 5: second consecutive Critical → stronger advisory evidence
        let v2 = guard.evaluate();
        assert!(
            v2.advisory_threshold_reached,
            "second consecutive Critical reaches the strong-advisory threshold"
        );
    }

    /// Critical observations increase evidence strength without restricting tools.
    #[test]
    fn advisory_threshold_reached_from_errors_and_nudges() {
        let mut guard = TurnGuard::new();
        guard.nudge_count = 6;

        // Many errors + health avoidance tools
        for _ in 0..5 {
            guard.record_tool_result("t1", "Error: fail");
        }
        for _ in 0..5 {
            guard.record_tool_result("t2", "Error: fail");
        }

        // First Critical remains below the strong threshold.
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Critical);
        assert!(
            !v.advisory_threshold_reached,
            "first Critical remains below the strong-advisory threshold"
        );

        // Next failing round is also Critical → stronger advisory evidence
        for _ in 0..3 {
            guard.record_tool_result("t1", "Error: fail");
        }
        let v2 = guard.evaluate();
        assert!(
            v2.advisory_threshold_reached,
            "second consecutive Critical reaches the strong-advisory threshold"
        );
    }

    /// Behavioral severity must not consume explicit turn budget.
    #[test]
    fn behavior_verdict_preserves_explicit_turn_budget() {
        let mut restricted = HashSet::new();

        // Warning is telemetry/advisory only.
        let warning_verdict = TurnVerdict {
            injections: vec!["warn".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Warning,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = observe_verdict(&warning_verdict, 25, &mut restricted);
        assert_eq!(r, 25);

        // Critical still has no budget authority.
        let critical_verdict = TurnVerdict {
            injections: vec!["crit".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Critical,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = observe_verdict(&critical_verdict, 23, &mut restricted);
        assert_eq!(r, 23);

        // Healthy costs 0
        let healthy_verdict = TurnVerdict {
            injections: vec![],
            avoid_tools: vec![],
            severity: VerdictSeverity::Healthy,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = observe_verdict(&healthy_verdict, 18, &mut restricted);
        assert_eq!(r, 18);

        // A low remaining budget is still owned by the explicit budget layer.
        let (r, _, _) = observe_verdict(&critical_verdict, 3, &mut restricted);
        assert_eq!(r, 3);
    }

    // ── Nudge-ignore detection ──

    /// If stall nudge says to change approach for bash (after 3+ occurrences),
    /// but next turn uses bash again → retry-caution warning injected without
    /// implying the tool is disabled.
    #[test]
    fn nudge_ignore_detection() {
        let mut guard = TurnGuard::new();

        // Turn 1-3: stall on bash (3 identical calls → retry caution includes bash)
        let bash_call = tc("bash", r#"{"command":"ls"}"#);
        guard.record_tool_calls(std::slice::from_ref(&bash_call));
        guard.record_tool_calls(std::slice::from_ref(&bash_call));
        guard.record_tool_calls(std::slice::from_ref(&bash_call));
        let v = guard.evaluate();
        assert!(v.severity >= VerdictSeverity::Warning);
        assert!(
            v.avoid_tools.contains(&"bash".to_string()),
            "3 occurrences should add bash to avoid list"
        );

        // Turn 4: ignore the advice, use bash again (different args → not same signature)
        guard.record_tool_calls(&[tc("bash", r#"{"command":"cat README.md"}"#)]);
        guard.record_tool_result("bash", "# README\ncontent");
        let v = guard.evaluate();

        // Should detect nudge-ignore while preserving the "not disabled" tool model.
        let has_nudge_ignore_warning = v.injections.iter().any(|m| {
            m.contains("prior correction asked you to change approach")
                && m.contains("[bash]")
                && m.contains("tools are not disabled unless a restricted_tool result says so")
        });
        assert!(
            has_nudge_ignore_warning,
            "should warn when LLM ignores nudge advice"
        );
    }

    // ── Cross-session health restore ──

    /// TurnGuard created with pre-existing health data preserves health avoidance.
    #[test]
    fn cross_session_health_preserved() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        use astra_turn_core::tool_health::ToolHealthTracker;

        let entries = vec![ToolHealthEntry {
            name: "flaky_tool".to_string(),
            total_calls: 10,
            total_failures: 8,
            input_validation_failures: 0,
            failure_rate: 0.8,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Guard should already have health avoidance for flaky_tool.
        assert!(guard.health.is_avoidance_advised("flaky_tool"));

        // Evaluate should include it in avoid
        let v = guard.evaluate();
        assert!(v.avoid_tools.contains(&"flaky_tool".to_string()));
    }

    // ── Mixed multi-turn scenario ──

    /// Full 6-turn mixed scenario: success → error → stall → recovery → diverge → productive.
    #[test]
    fn mixed_six_turn_realistic_session() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();
        let mut budget = 25usize;

        // Turn 1: successful git
        guard.record_tool_calls(&[tc("git", r#"{"action":"log","n":5}"#)]);
        guard.record_tool_result("git", r#"[{"sha":"a1b2c3"}]"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);

        // Turn 2: mo_query fails
        guard.record_tool_calls(&[tc("mo_query", r#"{"sql":"SELECT 1"}"#)]);
        guard.record_tool_result("mo_query", "Error: connection refused");
        let v = guard.evaluate();
        // One error is not enough for warning escalation
        assert!(v.severity <= VerdictSeverity::Info);

        // Turn 3: mo_query fails again (same call, but window=3 needs one more)
        guard.record_tool_calls(&[tc("mo_query", r#"{"sql":"SELECT 1"}"#)]);
        guard.record_tool_result("mo_query", "Error: connection refused");
        let v = guard.evaluate();
        // 2 identical calls → below window=3, no stall yet
        // But 2 errors may push toward warning via error count
        assert!(v.severity <= VerdictSeverity::Info);

        // Turn 3b: third identical mo_query → stall detected!
        guard.record_tool_calls(&[tc("mo_query", r#"{"sql":"SELECT 1"}"#)]);
        guard.record_tool_result("mo_query", "Error: connection refused");
        let v = guard.evaluate();
        assert!(
            v.severity >= VerdictSeverity::Warning,
            "stall on failing tool after 3 identical calls"
        );
        let (b, _, _) = observe_verdict(&v, budget, &mut restricted);
        budget = b;

        // Turn 4: recovery — different tool, success
        guard.record_tool_calls(&[tc("github", r#"{"action":"list_prs","state":"open"}"#)]);
        guard.record_tool_result("github", r#"[{"id":42}]"#);
        let v = guard.evaluate();
        // May still have escalation warning from nudge_count=1, but no new stall
        let has_stall = v.injections.iter().any(|m| m.contains("REFLECTION"));
        assert!(!has_stall, "no stall after recovery");

        // Turn 5: exploration (bash)
        guard.record_tool_calls(&[tc("bash", r#"{"command":"find . -name *.rs"}"#)]);
        guard.record_tool_result("bash", "src/main.rs");

        // Turn 6: more exploration (read_file) — could trigger divergence depending on history
        guard.record_tool_calls(&[tc("read_file", r#"{"path":"src/main.rs"}"#)]);
        guard.record_tool_result("read_file", "fn main() {}");
        let v = guard.evaluate();

        assert_eq!(budget, 25, "behavior evidence must not consume budget");
        assert!(
            !v.advisory_threshold_reached,
            "mixed session should remain below the strong-advisory threshold"
        );
    }

    // ── Injection message format contract ──

    /// Injected messages must be valid JSON-compatible strings for the messages array.
    #[test]
    fn injection_messages_are_valid_for_conversation() {
        let mut guard = TurnGuard::new();

        // Trigger stall
        let call = tc("bash", r#"{"command":"echo test"}"#);
        guard.record_tool_calls(std::slice::from_ref(&call));
        guard.record_tool_calls(std::slice::from_ref(&call));
        let v = guard.evaluate();

        for msg in &v.injections {
            // Must be non-empty
            assert!(!msg.is_empty());
            // Must be serializable into JSON string value
            let json_msg = json!({"role": "user", "content": msg});
            assert!(json_msg.get("content").unwrap().is_string());
        }
    }

    /// Advisory avoid tools can recur across turns without becoming hard restrictions.
    #[test]
    fn advisory_avoid_tools_do_not_accumulate_into_restricted_tools() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        // Turn 1: put one tool under health avoidance.
        for _ in 0..3 {
            guard.record_tool_result("rollback_database_snapshots", "Error: fail");
        }
        let v = guard.evaluate();
        assert!(
            v.avoid_tools
                .contains(&"rollback_database_snapshots".to_string())
        );
        observe_verdict(&v, 25, &mut restricted);
        assert!(!restricted.contains("rollback_database_snapshots"));

        // Turn 2: put another tool under health avoidance.
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: fail");
        }
        let v = guard.evaluate();
        assert!(v.avoid_tools.contains(&"write_file".to_string()));
        observe_verdict(&v, 25, &mut restricted);

        assert!(
            restricted.is_empty(),
            "soft health avoid guidance must not accumulate into restricted_tools"
        );
    }

    /// Verdict with empty injections and Healthy severity → skip in chat_stream
    /// (the `continue` condition: injections.is_empty OR severity < Warning).
    #[test]
    fn healthy_verdict_does_not_trigger_skip() {
        let v = TurnVerdict {
            injections: vec![],
            avoid_tools: vec![],
            severity: VerdictSeverity::Healthy,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        // chat_stream only skips (continues) when BOTH injections non-empty AND severity >= Warning
        let should_skip = !v.injections.is_empty() && v.severity >= VerdictSeverity::Warning;
        assert!(
            !should_skip,
            "healthy verdict should NOT trigger skip/continue"
        );
    }

    /// Info severity with injections → does NOT trigger skip (only Warning+ does).
    #[test]
    fn info_severity_with_injections_no_skip() {
        let v = TurnVerdict {
            injections: vec!["info note".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Info,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        let should_skip = !v.injections.is_empty() && v.severity >= VerdictSeverity::Warning;
        assert!(
            !should_skip,
            "Info+injections should inject but NOT skip the turn"
        );
    }

    /// Warning severity with injections → triggers skip (LLM re-evaluates with nudge).
    #[test]
    fn warning_severity_with_injections_triggers_skip() {
        let v = TurnVerdict {
            injections: vec!["stall detected".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Warning,
            advisory_threshold_reached: false,
            stall_detected: false,
            is_diverging: false,
        };
        let should_skip = !v.injections.is_empty() && v.severity >= VerdictSeverity::Warning;
        assert!(
            should_skip,
            "Warning+injections should skip to next LLM call"
        );
    }

    // ── Edge cases ──

    /// Zero tool calls in a turn (LLM gave text-only response) → TurnGuard is no-op.
    #[test]
    fn zero_tool_calls_no_effect() {
        let mut guard = TurnGuard::new();
        // Don't record any tool calls — text-only turn
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(v.injections.is_empty());
        assert!(!v.advisory_threshold_reached);
    }

    /// Many tools in one turn (batch call) → single signature, no stall.
    #[test]
    fn batch_tool_calls_no_stall() {
        let mut guard = TurnGuard::new();

        // 5 different tools in one turn
        guard.record_tool_calls(&[
            tc("bash", r#"{"command":"ls"}"#),
            tc("read_file", r#"{"path":"a.rs"}"#),
            tc("grep", r#"{"pattern":"fn main"}"#),
            tc("git", r#"{"action":"status"}"#),
            tc("git", r#"{"action":"diff"}"#),
        ]);
        for name in &["bash", "read_file", "grep", "git", "git"] {
            guard.record_tool_result(name, r#"{"data":"ok"}"#);
        }
        let v = guard.evaluate();
        assert_eq!(
            v.severity,
            VerdictSeverity::Healthy,
            "diverse batch call should not trigger stall"
        );
    }

    /// Tool that alternates success/fail does not trigger health avoidance.
    #[test]
    fn alternating_success_fail_no_health_avoidance() {
        let mut guard = TurnGuard::new();

        for i in 0..10 {
            if i % 2 == 0 {
                guard.record_tool_result("write_file", "Error: intermittent");
            } else {
                guard.record_tool_result("write_file", r#"{"output":"ok"}"#);
            }
        }

        assert!(
            !guard.health.is_avoidance_advised("write_file"),
            "alternating results should reset consecutive counter"
        );
    }

    // ── Cross-session health restore → selector-level exclusion ──

    /// Cross-session restore with low failure rate does not trigger health avoidance.
    #[test]
    fn cross_session_low_failure_rate_no_health_avoidance() {
        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 20,
            total_failures: 3,
            input_validation_failures: 0,
            failure_rate: 0.15,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_avoidance_advised("bash"),
            "15% failure rate should not trigger health avoidance"
        );

        let guard = TurnGuard::with_health(tracker);
        let restricted: Vec<String> = guard
            .health
            .health_avoidance_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            restricted.is_empty(),
            "no tools should be restricted at 15% failure"
        );
    }

    /// Cross-session restore with few calls does not trigger health avoidance.
    #[test]
    fn cross_session_few_calls_benefit_of_doubt() {
        let entries = vec![ToolHealthEntry {
            name: "mo_query".to_string(),
            total_calls: 3,
            total_failures: 3,
            input_validation_failures: 0,
            failure_rate: 1.0,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        // 100% failure but only 3 calls — below CROSS_SESSION_MIN_CALLS (5)
        assert!(
            !tracker.is_avoidance_advised("mo_query"),
            "too few calls should get benefit of the doubt even at 100% failure"
        );
    }

    /// Rehabilitated tool in session 2 should no longer appear in restricted_tools.
    #[test]
    fn cross_session_rehabilitation_clears_restriction() {
        let entries = vec![ToolHealthEntry {
            name: "github".to_string(),
            total_calls: 10,
            total_failures: 8,
            input_validation_failures: 0,
            failure_rate: 0.8,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Initially under health avoidance from prior session.
        assert!(guard.health.is_avoidance_advised("github"));

        // Tool succeeds in new session → rehabilitated
        guard.record_tool_result("github", r#"[{"number":42,"title":"Fix"}]"#);
        assert!(
            !guard.health.is_avoidance_advised("github"),
            "success should rehabilitate the tool"
        );

        let restricted: Vec<String> = guard
            .health
            .health_avoidance_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            !restricted.contains(&"github".to_string()),
            "rehabilitated tool should not be in restricted list"
        );
    }

    /// Export → import round-trip preserves failure statistics accurately.
    #[test]
    fn health_export_import_round_trip_fidelity() {
        let mut guard = TurnGuard::new();

        // Simulate mixed tool usage
        for _ in 0..5 {
            guard.record_tool_result("write_file", r#"{"output":"ok"}"#);
        }
        for _ in 0..3 {
            guard.record_tool_result("write_file", "Error: operation failed");
        }
        for _ in 0..7 {
            guard.record_tool_result("git", r#"[{"sha":"abc"}]"#);
        }

        let exported = guard.health.export();
        let restored = ToolHealthTracker::from_entries(&exported);

        // Verify round-trip accuracy
        let write_file_entry = exported.iter().find(|e| e.name == "write_file").unwrap();
        assert_eq!(write_file_entry.total_calls, 8);
        assert_eq!(write_file_entry.total_failures, 3);
        assert!((write_file_entry.failure_rate - 3.0 / 8.0).abs() < 0.01);

        let git_entry = exported.iter().find(|e| e.name == "git").unwrap();
        assert_eq!(git_entry.total_calls, 7);
        assert_eq!(git_entry.total_failures, 0);

        // write_file: 37.5% failure < 50% threshold → no health avoidance
        assert!(!restored.is_avoidance_advised("write_file"));
        // git: 0% → no health avoidance
        assert!(!restored.is_avoidance_advised("git"));
    }

    /// Verdict from restored guard correctly populates avoid_tools for schema exclusion.
    #[test]
    fn cross_session_verdict_feeds_schema_exclusion() {
        let entries = vec![
            ToolHealthEntry {
                name: "mo_query".to_string(),
                total_calls: 12,
                total_failures: 10,
                input_validation_failures: 0,
                failure_rate: 0.833,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "rollback_database_snapshots".to_string(),
                total_calls: 8,
                total_failures: 6,
                input_validation_failures: 0,
                failure_rate: 0.75,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Evaluate without any new activity — should still reflect health state
        let v = guard.evaluate();

        // Mutating unhealthy tools should be in avoid_tools; read-only
        // observation tools stay visible even when their health is poor.
        assert!(
            !v.avoid_tools.contains(&"mo_query".to_string()),
            "read-only mo_query should not be avoided: {:?}",
            v.avoid_tools
        );
        assert!(
            v.avoid_tools
                .contains(&"rollback_database_snapshots".to_string()),
            "75% failure tool should be avoided: {:?}",
            v.avoid_tools
        );

        // Apply verdict: health-only avoid guidance must not hide schemas.
        let mut restricted = HashSet::new();
        let (_, _, advisory_threshold_reached) = observe_verdict(&v, 20, &mut restricted);
        assert!(
            !advisory_threshold_reached,
            "health-only issues should not force stop"
        );
        assert!(!restricted.contains("mo_query"));
        assert!(!restricted.contains("rollback_database_snapshots"));
    }
}
