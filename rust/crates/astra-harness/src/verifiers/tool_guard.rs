use crate::{DecisionRecord, HookPoint, Severity, Verifier, Violation};

/// Complementary safety verifier that checks for dangerous tool usage patterns.
///
/// This does NOT replace the permission system — it adds a harness-level
/// observation layer that can warn or block based on session-level patterns.
#[derive(Default)]
pub struct ToolGuardVerifier {
    /// Tool names that should trigger a warning when called.
    pub warn_tools: Vec<String>,
    /// Maximum allowed tool calls per session before warning.
    pub max_tool_calls_per_session: Option<u32>,
}

impl Verifier for ToolGuardVerifier {
    fn name(&self) -> &'static str {
        "tool_guard"
    }

    fn trigger_points(&self) -> &'static [HookPoint] {
        &[HookPoint::PostToolBatch, HookPoint::PostTurn]
    }

    fn check(&self, record: &DecisionRecord) -> Vec<Violation> {
        let snap = &record.snapshot;
        let mut violations = Vec::new();

        if let Some(ref last_tool) = snap.last_tool_called
            && self.warn_tools.iter().any(|t| t == last_tool)
        {
            violations.push(Violation {
                severity: Severity::Warning,
                verifier: self.name().to_string(),
                message: format!("sensitive tool invoked: {last_tool}"),
            });
        }

        if let Some(max) = self.max_tool_calls_per_session
            && snap.tool_calls_this_session > max
        {
            violations.push(Violation {
                severity: Severity::Fatal,
                verifier: self.name().to_string(),
                message: format!(
                    "session tool call limit exceeded: {} / {}",
                    snap.tool_calls_this_session, max
                ),
            });
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeSnapshot;

    fn record_with(last_tool: Option<&str>, tool_calls: u32) -> DecisionRecord {
        DecisionRecord {
            session_id: "test".into(),
            turn: 1,
            point: HookPoint::PostToolBatch,
            wall_time_unix_millis: 0,
            monotonic_millis_since_session: 0,
            snapshot: RuntimeSnapshot {
                last_tool_called: last_tool.map(|s| s.to_string()),
                tool_calls_this_session: tool_calls,
                ..RuntimeSnapshot::empty()
            },
        }
    }

    #[test]
    fn no_violations_when_unconfigured() {
        let v = ToolGuardVerifier::default();
        assert!(v.check(&record_with(Some("bash"), 10)).is_empty());
    }

    #[test]
    fn warns_on_sensitive_tool() {
        let v = ToolGuardVerifier {
            warn_tools: vec!["execute_sql".into(), "delete_file".into()],
            max_tool_calls_per_session: None,
        };
        let violations = v.check(&record_with(Some("execute_sql"), 1));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Warning);
        assert!(violations[0].message.contains("execute_sql"));
    }

    #[test]
    fn no_warn_on_safe_tool() {
        let v = ToolGuardVerifier {
            warn_tools: vec!["execute_sql".into()],
            max_tool_calls_per_session: None,
        };
        assert!(v.check(&record_with(Some("read_file"), 1)).is_empty());
    }

    #[test]
    fn error_on_tool_call_limit() {
        let v = ToolGuardVerifier {
            warn_tools: vec![],
            max_tool_calls_per_session: Some(50),
        };
        let violations = v.check(&record_with(None, 51));
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Fatal);
    }

    #[test]
    fn within_limit_no_error() {
        let v = ToolGuardVerifier {
            warn_tools: vec![],
            max_tool_calls_per_session: Some(50),
        };
        assert!(v.check(&record_with(None, 50)).is_empty());
    }

    #[test]
    fn combined_violations() {
        let v = ToolGuardVerifier {
            warn_tools: vec!["bash".into()],
            max_tool_calls_per_session: Some(10),
        };
        let violations = v.check(&record_with(Some("bash"), 15));
        assert_eq!(violations.len(), 2);
    }
}
