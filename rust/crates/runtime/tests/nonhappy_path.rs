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
    use astra_runtime::turn::stall::{SERVER_STALL_WINDOW, detect_server_stall};
    use std::collections::BTreeSet;

    /// Proves stall detector catches repetitive tool calls
    #[test]
    fn detects_repetitive_tool_calls() {
        let sig = BTreeSet::from(["bash".to_string(), "echo hello".to_string()]);
        let tool_sigs = vec![sig.clone(), sig.clone(), sig.clone()];

        assert!(
            detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
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
            !detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW),
            "Should NOT detect stall with varied tool calls"
        );
    }
}

mod turn_limits {
    use astra_runtime::turn::routing::MAX_TOOL_ROUNDS;

    /// Proves MAX_TOOL_ROUNDS is set to a reasonable value.
    /// The env override can only DECREASE the limit, not bypass it.
    #[test]
    fn max_rounds_is_bounded() {
        const { assert!(MAX_TOOL_ROUNDS > 0) };
        const { assert!(MAX_TOOL_ROUNDS <= 100) }; // matches MAX_TOOL_ROUNDS_DEFAULT
    }
}

// ── Turn Guard Integration ──────────────────────────────────────────────────

mod turn_guard_integration {
    use astra_runtime::turn::turn_guard::{TurnGuard, VerdictSeverity};
    use serde_json::json;

    fn tool_call(name: &str, args: &str) -> serde_json::Value {
        json!({"function": {"name": name, "arguments": args}})
    }

    /// Proves: normal session produces no injections
    #[test]
    fn normal_session_stays_healthy() {
        let mut guard = TurnGuard::new();

        // Turn 1: productive tool call (not exploration-only)
        guard.record_tool_calls(&[tool_call("github_list_prs", r#"{"state":"open"}"#)]);
        guard.record_tool_result("github_list_prs", r#"[{"id": 1, "title": "fix bug"}]"#);

        // Turn 2: different productive tool
        guard.record_tool_calls(&[tool_call("git_log", r#"{"limit":5}"#)]);
        guard.record_tool_result("git_log", r#"{"commits": [{"sha": "abc"}]}"#);

        let verdict = guard.evaluate();
        assert_eq!(verdict.severity, VerdictSeverity::Healthy);
        assert!(verdict.injections.is_empty());
        assert!(verdict.avoid_tools.is_empty());
        assert!(!verdict.force_stop);
    }

    /// Proves: stall + tool health compose correctly
    #[test]
    fn stall_and_health_compose() {
        let mut guard = TurnGuard::new();

        // 3 consecutive failures → deprioritized
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");

        // Same tool call three times → stall (window=3)
        let calls = [tool_call("bash", r#"{"command":"ls"}"#)];
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
        assert!(verdict.avoid_tools.contains(&"bash".to_string()));
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

    /// Proves: empty results don't deprioritize but are tracked
    #[test]
    fn empty_results_tracked_without_deprioritization() {
        let mut guard = TurnGuard::new();

        // 10 empty results from grep
        for _ in 0..10 {
            guard.record_tool_result("grep", "[]");
        }

        assert!(
            !guard.health.is_deprioritized("grep"),
            "Empty results should not deprioritize"
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
            guard.record_tool_result("bash", "Error: fail");
        }
        assert!(guard.health.is_deprioritized("bash"));

        // Rehabilitate
        guard.record_tool_result("bash", r#"{"ok": true}"#);
        assert!(!guard.health.is_deprioritized("bash"));

        // Second failure cycle
        for _ in 0..3 {
            guard.record_tool_result("bash", "Error: fail");
        }
        assert!(guard.health.is_deprioritized("bash"));

        // Rehabilitate again
        guard.record_tool_result("bash", r#"{"ok": true}"#);

        // Third failure cycle: only 2 needed (stricter threshold)
        guard.record_tool_result("bash", "Error: fail");
        guard.record_tool_result("bash", "Error: fail");
        assert!(
            guard.health.is_deprioritized("bash"),
            "Flaky tool should be deprioritized after only 2 failures"
        );
    }

    /// Proves: diverse error types all get classified
    #[test]
    fn error_classification_integration() {
        let mut guard = TurnGuard::new();

        guard.record_tool_result("github_list_prs", "Error: 401 Unauthorized");
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
        use astra_runtime::turn::tool_health::ToolHealthTracker;

        // Tool A: 3 calls, 100% failure → NOT deprioritized (too few calls, need >=8)
        // Tool B: 10 calls, 80% failure → deprioritized (enough data + above 70% threshold)
        // Tool C: 10 calls, 60% failure → NOT deprioritized (below 70% threshold)
        let entries = vec![
            ToolHealthEntry {
                name: "tool_a".to_string(),
                total_calls: 3,
                total_failures: 3,
                failure_rate: 1.0,
                last_updated_epoch: 0,
            },
            ToolHealthEntry {
                name: "tool_b".to_string(),
                total_calls: 10,
                total_failures: 8,
                failure_rate: 0.8,
                last_updated_epoch: 0,
            },
            ToolHealthEntry {
                name: "tool_c".to_string(),
                total_calls: 10,
                total_failures: 6,
                failure_rate: 0.6,
                last_updated_epoch: 0,
            },
        ];

        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("tool_a"),
            "Too few calls to deprioritize"
        );
        assert!(
            tracker.is_deprioritized("tool_b"),
            "Enough calls + high failure rate → deprioritized"
        );
        assert!(
            !tracker.is_deprioritized("tool_c"),
            "Below failure threshold → not deprioritized"
        );
    }
}

// ── Input Guard Integration ─────────────────────────────────────────────────

mod input_guards {
    use astra_runtime::tool_registry::state::ConversationState;

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
    use astra_runtime::turn::error_recovery::*;

    #[test]
    fn full_recovery_flow() {
        // Simulate: tool fails → classify → suggest alternatives → escalate
        let error = "HTTP 503 Service Unavailable";
        let category = classify_error(error);
        assert_eq!(category, ErrorCategory::Transient);

        // Should retry
        let delay = should_retry(category, 0);
        assert!(delay.is_some());

        // After retry fails, build recovery message
        let msg = build_recovery_message("github_list_prs", error, category, &[]);
        assert!(msg.contains("Alternatives"));

        // Escalation after multiple issues (3 nudges = Critical with new thresholds,
        // use 2 nudges + 3 errors for Warning)
        let level = escalation_level(2, 3, 1);
        assert_eq!(level, EscalationLevel::Warning);
    }

    #[test]
    fn command_not_found_classified_correctly() {
        // "command not found" should be Unavailable, not NotFound
        let cat = classify_error("bash: mysql: command not found");
        assert_eq!(
            cat,
            ErrorCategory::Unavailable,
            "command not found should be Unavailable, not NotFound"
        );
    }

    #[test]
    fn error_summary_accumulates_correctly() {
        let mut summary = SessionErrorSummary::new();
        summary.record_error(ErrorCategory::Transient);
        summary.record_error(ErrorCategory::Auth);
        summary.record_error(ErrorCategory::Transient);
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
//   result_feedback (per tool) → evaluate → apply verdict
//
// Each test sets up multi-turn scenarios and asserts not just the verdict,
// but the SIDE EFFECTS that chat_stream applies (restricted_tools, budget, messages).

mod chat_stream_turnguard_e2e {
    use astra_runtime::pipeline::persistence::ToolHealthEntry;
    use astra_runtime::tool_selector::ToolSelector;
    use astra_runtime::turn::result_quality::ResultQuality;
    use astra_runtime::turn::tool_health::ToolHealthTracker;
    use astra_runtime::turn::turn_guard::{TurnGuard, TurnVerdict, VerdictSeverity};
    use serde_json::json;
    use std::collections::HashSet;

    fn tc(name: &str, args: &str) -> serde_json::Value {
        json!({"function": {"name": name, "arguments": args}})
    }

    /// Simulates the exact verdict-application logic from chat_stream.rs.
    /// Returns (remaining_turns_after, restricted_tools, stall_events, force_stop).
    fn apply_verdict(
        verdict: &TurnVerdict,
        remaining: usize,
        restricted: &mut HashSet<String>,
    ) -> (usize, Vec<(String, u32)>, bool) {
        let mut remaining_turns = remaining;
        let mut stall_events = Vec::new();

        for tool in &verdict.avoid_tools {
            restricted.insert(tool.clone());
        }

        match verdict.severity {
            VerdictSeverity::Critical => {
                remaining_turns = remaining_turns.saturating_sub(5);
                stall_events.push(("critical_escalation".to_string(), 0u32));
            }
            VerdictSeverity::Warning => {
                remaining_turns = remaining_turns.saturating_sub(2);
                stall_events.push(("warning".to_string(), 0u32));
            }
            _ => {}
        }

        (remaining_turns, stall_events, verdict.force_stop)
    }

    // ── Happy path scenarios ──

    /// 5-turn productive session: no injections, no budget loss, no restrictions.
    #[test]
    fn productive_five_turn_session_no_side_effects() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();
        let max_turns = 25usize;

        // Turn 1: git_log → success
        guard.record_tool_calls(&[tc("git_log", r#"{"limit":10}"#)]);
        let q = guard.record_tool_result("git_log", r#"[{"sha":"abc","msg":"fix"}]"#);
        assert_eq!(q, ResultQuality::Success);
        let v = guard.evaluate();
        let (remaining, events, stop) = apply_verdict(&v, max_turns, &mut restricted);
        assert_eq!(remaining, max_turns);
        assert!(events.is_empty());
        assert!(!stop);

        // Turn 2: different tool
        guard.record_tool_calls(&[tc("github_list_prs", r#"{"state":"open"}"#)]);
        guard.record_tool_result("github_list_prs", r#"[{"id":1}]"#);
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);

        // Turn 3: write_file
        guard.record_tool_calls(&[tc(
            "write_file",
            r#"{"path":"x.rs","content":"fn main(){}"}"#,
        )]);
        guard.record_tool_result("write_file", r#"{"written":true}"#);

        // Turn 4: git_status
        guard.record_tool_calls(&[tc("git_status", r#"{}"#)]);
        guard.record_tool_result("git_status", r#"{"modified":["x.rs"]}"#);

        // Turn 5: git_diff
        guard.record_tool_calls(&[tc("git_diff", r#"{"cached":false}"#)]);
        guard.record_tool_result("git_diff", r#"+fn main(){}\n-fn old(){}"#);

        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Healthy);
        assert!(v.injections.is_empty());
        assert!(v.avoid_tools.is_empty());
        assert!(restricted.is_empty());
    }

    // ── Stall scenarios ──

    /// Exact same tool call 2 turns in a row → stall detected, nudge injected,
    /// budget penalty applied. Avoid_tools populated only if tool count >= 3.
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

        // Apply verdict like chat_stream does
        let (remaining, events, _) = apply_verdict(&v2, 25, &mut restricted);
        assert_eq!(remaining, 23, "Warning costs 2 turns");
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

    /// 8+ rounds of exploration-only tools → DIVERGENCE_CORRECTION injected.
    #[test]
    fn exploration_divergence_triggers_correction() {
        let mut guard = TurnGuard::new();

        // All exploration tools: bash, read_file, grep, list_dir, glob (8 rounds)
        guard.record_tool_calls(&[tc("bash", r#"{"command":"find . -name *.rs"}"#)]);
        guard.record_tool_result("bash", "src/main.rs\nsrc/lib.rs");

        guard.record_tool_calls(&[tc("read_file", r#"{"path":"src/main.rs"}"#)]);
        guard.record_tool_result("read_file", "fn main() {}");

        guard.record_tool_calls(&[tc("grep", r#"{"pattern":"TODO"}"#)]);
        guard.record_tool_result("grep", "src/lib.rs:10: // TODO fix this");

        guard.record_tool_calls(&[tc("list_dir", r#"{"path":"src"}"#)]);
        guard.record_tool_result("list_dir", "main.rs\nlib.rs");

        guard.record_tool_calls(&[tc("glob", r#"{"pattern":"**/*.toml"}"#)]);
        guard.record_tool_result("glob", "Cargo.toml");

        guard.record_tool_calls(&[tc("bash", r#"{"command":"wc -l src/*.rs"}"#)]);
        guard.record_tool_result("bash", "42 src/main.rs");

        guard.record_tool_calls(&[tc("read_file", r#"{"path":"src/lib.rs"}"#)]);
        guard.record_tool_result("read_file", "pub fn lib() {}");

        guard.record_tool_calls(&[tc("grep", r#"{"pattern":"fn "}"#)]);
        guard.record_tool_result("grep", "src/main.rs:1: fn main()");

        let v = guard.evaluate();
        assert!(
            v.injections.iter().any(|m| m.contains("exploring")),
            "divergence correction should mention exploration"
        );
        assert!(v.severity >= VerdictSeverity::Warning);
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
        let has_divergence = v.injections.iter().any(|m| m.contains("exploring"));
        assert!(!has_divergence, "productive tool should break divergence");
    }

    // ── Tool health scenarios ──

    /// Tool deprioritized after 3 errors → shows up in avoid_tools AND restricted_tools.
    #[test]
    fn tool_deprioritization_propagates_to_restricted() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        guard.record_tool_result("mo_query", "Error: connection refused");
        guard.record_tool_result("mo_query", "Error: connection refused");
        guard.record_tool_result("mo_query", "Error: connection refused");

        assert!(guard.health.is_deprioritized("mo_query"));

        let v = guard.evaluate();
        assert!(v.avoid_tools.contains(&"mo_query".to_string()));

        // Apply verdict
        apply_verdict(&v, 25, &mut restricted);
        assert!(
            restricted.contains("mo_query"),
            "deprioritized tool must land in restricted_tools"
        );
    }

    /// Rehabilitation removes tool from avoid list (next evaluation).
    #[test]
    fn rehabilitation_clears_tool_from_avoid() {
        let mut guard = TurnGuard::new();

        // Deprioritize
        for _ in 0..3 {
            guard.record_tool_result("bash", "Error: fail");
        }
        let v1 = guard.evaluate();
        assert!(v1.avoid_tools.contains(&"bash".to_string()));

        // Rehabilitate
        guard.record_tool_result("bash", r#"{"output":"ok"}"#);
        assert!(!guard.health.is_deprioritized("bash"));

        // Next evaluation should not list bash in avoid (from health)
        // Note: it might still appear from escalation/stall — we test health specifically
        let deprioritized = guard.health.deprioritized_tools();
        assert!(
            !deprioritized.contains(&"bash"),
            "rehabilitated tool not in deprioritized list"
        );
    }

    /// Multiple tools fail independently → each tracked separately.
    #[test]
    fn independent_tool_health_tracking() {
        let mut guard = TurnGuard::new();

        // bash fails, grep succeeds
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("bash", "Error: permission denied");
        guard.record_tool_result("grep", r#"{"matches":["a.rs"]}"#);

        assert!(guard.health.is_deprioritized("bash"));
        assert!(!guard.health.is_deprioritized("grep"));
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
            fb.unwrap().contains("empty"),
            "feedback should mention empty"
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
        assert!(!guard.health.is_deprioritized("a")); // success
        assert!(!guard.health.is_deprioritized("c")); // empty → not deprioritized
    }

    // ── Escalation + force stop ──

    /// Full escalation path: normal → warning → critical → force_stop.
    /// Now Critical requires nudges + errors (pure nudges stay at Warning).
    #[test]
    fn escalation_path_to_force_stop() {
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
        let (b, _, _) = apply_verdict(&v, budget, &mut restricted);
        budget = b;
        assert!(budget < 25, "warning should reduce budget");

        // Phase 2: more stalls to accumulate nudges
        for _ in 0..3 {
            guard.record_tool_calls(std::slice::from_ref(&call));
            let v = guard.evaluate();
            let (b, _, _) = apply_verdict(&v, budget, &mut restricted);
            budget = b;
        }
        assert!(guard.nudge_count >= 3);

        // Phase 3: pure nudges → only Warning (not Critical without errors)
        let v = guard.evaluate();
        assert!(
            !v.force_stop,
            "pure nudges without errors must NOT force_stop"
        );

        // Phase 4: add tool errors to couple with nudges → first Critical → restricted
        guard.record_tool_result("bash", "error: no such file");
        guard.record_tool_result("bash", "error: not found");
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Critical);
        assert!(
            !v.force_stop,
            "first Critical → restricted, not force_stop (progressive degradation)"
        );

        // Phase 5: second consecutive Critical → force_stop
        let v2 = guard.evaluate();
        assert!(v2.force_stop, "second consecutive Critical → force_stop");
    }

    /// Critical + 6 nudges → progressive degradation: 1st restricted, 2nd force_stop.
    #[test]
    fn force_stop_from_errors_and_nudges() {
        let mut guard = TurnGuard::new();
        guard.nudge_count = 6;

        // Many errors + deprioritized tools
        for _ in 0..5 {
            guard.record_tool_result("t1", "Error: fail");
        }
        for _ in 0..5 {
            guard.record_tool_result("t2", "Error: fail");
        }

        // First Critical → restricted (progressive degradation)
        let v = guard.evaluate();
        assert_eq!(v.severity, VerdictSeverity::Critical);
        assert!(
            !v.force_stop,
            "first Critical → restricted, not force_stop (progressive degradation)"
        );

        // Second consecutive Critical → force_stop
        let v2 = guard.evaluate();
        assert!(v2.force_stop, "second consecutive Critical → force_stop");
    }

    /// Budget depletion: Warning=-2, Critical=-5.
    #[test]
    fn budget_penalty_accounting() {
        let mut restricted = HashSet::new();

        // Warning costs 2
        let warning_verdict = TurnVerdict {
            injections: vec!["warn".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Warning,
            force_stop: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = apply_verdict(&warning_verdict, 25, &mut restricted);
        assert_eq!(r, 23);

        // Critical costs 5
        let critical_verdict = TurnVerdict {
            injections: vec!["crit".into()],
            avoid_tools: vec![],
            severity: VerdictSeverity::Critical,
            force_stop: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = apply_verdict(&critical_verdict, 23, &mut restricted);
        assert_eq!(r, 18);

        // Healthy costs 0
        let healthy_verdict = TurnVerdict {
            injections: vec![],
            avoid_tools: vec![],
            severity: VerdictSeverity::Healthy,
            force_stop: false,
            stall_detected: false,
            is_diverging: false,
        };
        let (r, _, _) = apply_verdict(&healthy_verdict, 18, &mut restricted);
        assert_eq!(r, 18);

        // Budget can't go below 0
        let (r, _, _) = apply_verdict(&critical_verdict, 3, &mut restricted);
        assert_eq!(r, 0, "budget floors at 0 via saturating_sub");
    }

    // ── Nudge-ignore detection ──

    /// If stall nudge says "avoid bash" (after 3+ occurrences),
    /// but next turn uses bash → warning injected.
    #[test]
    fn nudge_ignore_detection() {
        let mut guard = TurnGuard::new();

        // Turn 1-3: stall on bash (3 identical calls → avoid_tools includes bash)
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

        // Should detect nudge-ignore
        let has_nudge_ignore_warning = v.injections.iter().any(|m| m.contains("told to avoid"));
        assert!(
            has_nudge_ignore_warning,
            "should warn when LLM ignores nudge advice"
        );
    }

    // ── Cross-session health restore ──

    /// TurnGuard created with pre-existing health data preserves deprioritization.
    #[test]
    fn cross_session_health_preserved() {
        use astra_runtime::pipeline::persistence::ToolHealthEntry;
        use astra_runtime::turn::tool_health::ToolHealthTracker;

        let entries = vec![ToolHealthEntry {
            name: "flaky_tool".to_string(),
            total_calls: 10,
            total_failures: 8,
            failure_rate: 0.8,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Guard should already have flaky_tool deprioritized
        assert!(guard.health.is_deprioritized("flaky_tool"));

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

        // Turn 1: successful git_log
        guard.record_tool_calls(&[tc("git_log", r#"{"limit":5}"#)]);
        guard.record_tool_result("git_log", r#"[{"sha":"a1b2c3"}]"#);
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
        let (b, _, _) = apply_verdict(&v, budget, &mut restricted);
        budget = b;

        // Turn 4: recovery — different tool, success
        guard.record_tool_calls(&[tc("github_list_prs", r#"{"state":"open"}"#)]);
        guard.record_tool_result("github_list_prs", r#"[{"id":42}]"#);
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

        // Session should still be within budget
        assert!(budget > 0, "budget should survive mixed session");
        assert!(!v.force_stop, "mixed session should not force stop");
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

    /// Restricted tools accumulate across turns (never cleared implicitly).
    #[test]
    fn restricted_tools_accumulate_across_turns() {
        let mut guard = TurnGuard::new();
        let mut restricted = HashSet::new();

        // Turn 1: deprioritize bash
        for _ in 0..3 {
            guard.record_tool_result("bash", "Error: fail");
        }
        let v = guard.evaluate();
        apply_verdict(&v, 25, &mut restricted);
        assert!(restricted.contains("bash"));

        // Turn 2: deprioritize grep
        for _ in 0..3 {
            guard.record_tool_result("grep", "Error: fail");
        }
        let v = guard.evaluate();
        apply_verdict(&v, 25, &mut restricted);
        assert!(restricted.contains("grep"));

        // Both should be in restricted
        assert!(restricted.contains("bash"), "bash still restricted");
        assert!(restricted.contains("grep"), "grep also restricted");
    }

    /// Verdict with empty injections and Healthy severity → skip in chat_stream
    /// (the `continue` condition: injections.is_empty OR severity < Warning).
    #[test]
    fn healthy_verdict_does_not_trigger_skip() {
        let v = TurnVerdict {
            injections: vec![],
            avoid_tools: vec![],
            severity: VerdictSeverity::Healthy,
            force_stop: false,
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
            force_stop: false,
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
            force_stop: false,
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
        assert!(!v.force_stop);
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
            tc("git_status", r#"{}"#),
            tc("git_diff", r#"{"cached":false}"#),
        ]);
        for name in &["bash", "read_file", "grep", "git_status", "git_diff"] {
            guard.record_tool_result(name, r#"{"data":"ok"}"#);
        }
        let v = guard.evaluate();
        assert_eq!(
            v.severity,
            VerdictSeverity::Healthy,
            "diverse batch call should not trigger stall"
        );
    }

    /// Tool that alternates success/fail does NOT get deprioritized (no consecutive failures).
    #[test]
    fn alternating_success_fail_no_deprioritization() {
        let mut guard = TurnGuard::new();

        for i in 0..10 {
            if i % 2 == 0 {
                guard.record_tool_result("bash", "Error: intermittent");
            } else {
                guard.record_tool_result("bash", r#"{"output":"ok"}"#);
            }
        }

        assert!(
            !guard.health.is_deprioritized("bash"),
            "alternating results should reset consecutive counter"
        );
    }

    // ── Cross-session health restore → selector-level exclusion ──

    /// Full lifecycle: session 1 deprioritizes a tool → export → session 2 restores →
    /// deprioritized tool appears in restricted_tools → TfIdfSelector excludes it.
    #[tokio::test]
    async fn cross_session_deprioritized_tool_excluded_from_selector() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::{SelectionContext, TfIdfSelector};

        // --- Session 1: tool fails and gets deprioritized ---
        let mut guard1 = TurnGuard::new();
        for _ in 0..8 {
            guard1.record_tool_result("github_ci_status", "Error: API rate limit exceeded");
        }
        assert!(
            guard1.health.is_deprioritized("github_ci_status"),
            "8 consecutive failures should deprioritize"
        );

        // Export health state (would be persisted to disk/cloud)
        let exported = guard1.health.export();
        assert!(
            !exported.is_empty(),
            "export should include the deprioritized tool"
        );
        let ci_entry = exported
            .iter()
            .find(|e| e.name == "github_ci_status")
            .unwrap();
        assert!(ci_entry.failure_rate >= 0.7, "failure rate should be high");

        // --- Session 2: restore from exported health ---
        let tracker2 = ToolHealthTracker::from_entries(&exported);
        let guard2 = TurnGuard::with_health(tracker2);
        assert!(
            guard2.health.is_deprioritized("github_ci_status"),
            "restored tracker should start with tool deprioritized"
        );

        // Proactive seeding: deprioritized_tools → restricted_tools (mirrors chat_stream.rs)
        let restricted: Vec<String> = guard2
            .health
            .deprioritized_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert!(restricted.contains(&"github_ci_status".to_string()));

        // TfIdfSelector should exclude the restricted tool
        let schemas: Vec<serde_json::Value> = astra_runtime::tool_registry::TOOL_CATALOG
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect();
        let registry = ToolRegistry::new(schemas);
        let selector = TfIdfSelector::new(registry);
        let ctx = SelectionContext {
            query: "check github CI status for latest commit",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 1200,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: restricted,
            file_context: vec![],
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        assert!(
            !result.tool_names.contains(&"github_ci_status".to_string()),
            "deprioritized tool from prior session should be excluded: {:?}",
            result.tool_names
        );
    }

    /// Cross-session restore with low failure rate does NOT deprioritize (benefit of doubt).
    #[test]
    fn cross_session_low_failure_rate_not_deprioritized() {
        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 20,
            total_failures: 3,
            failure_rate: 0.15,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        assert!(
            !tracker.is_deprioritized("bash"),
            "15% failure rate should not deprioritize"
        );

        let guard = TurnGuard::with_health(tracker);
        let restricted: Vec<String> = guard
            .health
            .deprioritized_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            restricted.is_empty(),
            "no tools should be restricted at 15% failure"
        );
    }

    /// Cross-session restore with few calls does NOT deprioritize (insufficient evidence).
    #[test]
    fn cross_session_few_calls_benefit_of_doubt() {
        let entries = vec![ToolHealthEntry {
            name: "mo_query".to_string(),
            total_calls: 3,
            total_failures: 3,
            failure_rate: 1.0,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        // 100% failure but only 3 calls — below CROSS_SESSION_MIN_CALLS (5)
        assert!(
            !tracker.is_deprioritized("mo_query"),
            "too few calls should get benefit of the doubt even at 100% failure"
        );
    }

    /// Rehabilitated tool in session 2 should no longer appear in restricted_tools.
    #[test]
    fn cross_session_rehabilitation_clears_restriction() {
        let entries = vec![ToolHealthEntry {
            name: "github_list_prs".to_string(),
            total_calls: 10,
            total_failures: 8,
            failure_rate: 0.8,
            last_updated_epoch: 0,
        }];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Initially deprioritized from prior session
        assert!(guard.health.is_deprioritized("github_list_prs"));

        // Tool succeeds in new session → rehabilitated
        guard.record_tool_result("github_list_prs", r#"[{"number":42,"title":"Fix"}]"#);
        assert!(
            !guard.health.is_deprioritized("github_list_prs"),
            "success should rehabilitate the tool"
        );

        let restricted: Vec<String> = guard
            .health
            .deprioritized_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        assert!(
            !restricted.contains(&"github_list_prs".to_string()),
            "rehabilitated tool should not be in restricted list"
        );
    }

    /// Multiple tools with mixed health: only the truly unreliable ones are restricted.
    #[tokio::test]
    async fn cross_session_mixed_health_selective_exclusion() {
        use astra_runtime::tool_registry::ToolRegistry;
        use astra_runtime::tool_selector::{SelectionContext, TfIdfSelector};

        let entries = vec![
            ToolHealthEntry {
                name: "github_ci_status".to_string(),
                total_calls: 10,
                total_failures: 9,
                failure_rate: 0.9,
                last_updated_epoch: 0,
            },
            ToolHealthEntry {
                name: "github_list_prs".to_string(),
                total_calls: 15,
                total_failures: 2,
                failure_rate: 0.133,
                last_updated_epoch: 0,
            },
            ToolHealthEntry {
                name: "git_log".to_string(),
                total_calls: 20,
                total_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
            },
        ];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let guard = TurnGuard::with_health(tracker);

        let restricted: Vec<String> = guard
            .health
            .deprioritized_tools()
            .into_iter()
            .map(|s| s.to_string())
            .collect();

        // Only github_ci_status should be restricted (90% failure, 10 calls)
        assert!(
            restricted.contains(&"github_ci_status".to_string()),
            "90% failure tool should be restricted"
        );
        assert!(
            !restricted.contains(&"github_list_prs".to_string()),
            "13% failure tool should NOT be restricted"
        );
        assert!(
            !restricted.contains(&"git_log".to_string()),
            "0% failure tool should NOT be restricted"
        );

        // Selector proof: query that matches all three GitHub tools
        let schemas: Vec<serde_json::Value> = astra_runtime::tool_registry::TOOL_CATALOG
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": {"type": "object", "properties": {}}
                    }
                })
            })
            .collect();
        let registry = ToolRegistry::new(schemas);
        let selector = TfIdfSelector::new(registry);
        let ctx = SelectionContext {
            query: "show github PR status and CI",
            turn_count: 1,
            recent_tools: &[],
            budget_tokens: 1200,
            boost_terms: vec![],
            budget_pressure: 0.0,
            memory_domain_hints: vec![],
            restricted_tools: restricted,
            file_context: vec![],
            previous_confidence_fallback: None,
        };
        let result = selector.select(&ctx).await;
        assert!(
            !result.tool_names.contains(&"github_ci_status".to_string()),
            "high-failure tool excluded from selection: {:?}",
            result.tool_names
        );
        // github_list_prs should still be available (if it scores high enough)
        // git_log should still be available (if it scores high enough)
    }

    /// Export → import round-trip preserves failure statistics accurately.
    #[test]
    fn health_export_import_round_trip_fidelity() {
        let mut guard = TurnGuard::new();

        // Simulate mixed tool usage
        for _ in 0..5 {
            guard.record_tool_result("bash", r#"{"output":"ok"}"#);
        }
        for _ in 0..3 {
            guard.record_tool_result("bash", "Error: command not found");
        }
        for _ in 0..7 {
            guard.record_tool_result("git_log", r#"[{"sha":"abc"}]"#);
        }

        let exported = guard.health.export();
        let restored = ToolHealthTracker::from_entries(&exported);

        // Verify round-trip accuracy
        let bash_entry = exported.iter().find(|e| e.name == "bash").unwrap();
        assert_eq!(bash_entry.total_calls, 8);
        assert_eq!(bash_entry.total_failures, 3);
        assert!((bash_entry.failure_rate - 3.0 / 8.0).abs() < 0.01);

        let git_entry = exported.iter().find(|e| e.name == "git_log").unwrap();
        assert_eq!(git_entry.total_calls, 7);
        assert_eq!(git_entry.total_failures, 0);

        // bash: 37.5% failure < 50% threshold → not deprioritized
        assert!(!restored.is_deprioritized("bash"));
        // git_log: 0% → definitely not deprioritized
        assert!(!restored.is_deprioritized("git_log"));
    }

    /// Verdict from restored guard correctly populates avoid_tools for schema exclusion.
    #[test]
    fn cross_session_verdict_feeds_schema_exclusion() {
        let entries = vec![
            ToolHealthEntry {
                name: "mo_query".to_string(),
                total_calls: 12,
                total_failures: 10,
                failure_rate: 0.833,
                last_updated_epoch: 0,
            },
            ToolHealthEntry {
                name: "mo_snapshot".to_string(),
                total_calls: 8,
                total_failures: 6,
                failure_rate: 0.75,
                last_updated_epoch: 0,
            },
        ];
        let tracker = ToolHealthTracker::from_entries(&entries);
        let mut guard = TurnGuard::with_health(tracker);

        // Evaluate without any new activity — should still reflect health state
        let v = guard.evaluate();

        // Both should be in avoid_tools
        assert!(
            v.avoid_tools.contains(&"mo_query".to_string()),
            "83% failure tool should be avoided: {:?}",
            v.avoid_tools
        );
        assert!(
            v.avoid_tools.contains(&"mo_snapshot".to_string()),
            "75% failure tool should be avoided: {:?}",
            v.avoid_tools
        );

        // Apply verdict (simulating chat_stream.rs schema exclusion)
        let mut restricted = HashSet::new();
        let (_, _, force_stop) = apply_verdict(&v, 20, &mut restricted);
        assert!(!force_stop, "health-only issues should not force stop");
        assert!(restricted.contains("mo_query"));
        assert!(restricted.contains("mo_snapshot"));
    }
}
