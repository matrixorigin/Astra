use std::sync::OnceLock;

use serde_json::Value;

const DESTRUCTIVE_KEYWORDS: &[&str] = &["DROP", "DELETE", "TRUNCATE", "ALTER", "GRANT", "REVOKE"];
const SHELL_EXECUTION_TOOLS: &[&str] = &["bash", "exec", "run_command", "shell"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyMiddlewareDecision {
    Allow,
    Deny(String),
}

#[derive(Clone, Copy)]
pub struct SafetyGuard {
    name: &'static str,
    evaluate: fn(&str, &Value) -> Option<String>,
}

impl SafetyGuard {
    pub const fn new(name: &'static str, evaluate: fn(&str, &Value) -> Option<String>) -> Self {
        Self { name, evaluate }
    }
}

#[derive(Clone)]
pub struct SafetyMiddleware {
    guards: Vec<SafetyGuard>,
}

impl Default for SafetyMiddleware {
    fn default() -> Self {
        Self::new(vec![
            SafetyGuard::new("destructive_sql", destructive_sql_guard),
            SafetyGuard::new("shell_obfuscation", shell_obfuscation_guard),
        ])
    }
}

impl SafetyMiddleware {
    #[must_use]
    pub fn new(guards: Vec<SafetyGuard>) -> Self {
        Self { guards }
    }

    #[must_use]
    pub fn evaluate(&self, tool_name: &str, tool_args: &Value) -> SafetyMiddlewareDecision {
        for guard in &self.guards {
            if let Some(reason) = (guard.evaluate)(tool_name, tool_args) {
                return SafetyMiddlewareDecision::Deny(format!(
                    "blocked by safety guard '{}': {reason}",
                    guard.name
                ));
            }
        }
        SafetyMiddlewareDecision::Allow
    }
}

#[must_use]
pub fn evaluate_tool_safety_request(
    tool_name: &str,
    tool_args: &Value,
) -> SafetyMiddlewareDecision {
    static DEFAULT_MIDDLEWARE: OnceLock<SafetyMiddleware> = OnceLock::new();
    DEFAULT_MIDDLEWARE
        .get_or_init(SafetyMiddleware::default)
        .evaluate(tool_name, tool_args)
}

fn destructive_sql_guard(tool_name: &str, tool_args: &Value) -> Option<String> {
    if tool_name != "mo_query" {
        return None;
    }
    let sql = tool_args.get("sql").and_then(Value::as_str)?;
    let allow_destructive = tool_args
        .get("allow_destructive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if allow_destructive {
        return None;
    }
    check_sql_safety(sql).map(|kind| {
        format!(
            "{kind} statements are blocked by default. Pass \"allow_destructive\": true to confirm execution."
        )
    })
}

fn shell_obfuscation_guard(tool_name: &str, tool_args: &Value) -> Option<String> {
    if !SHELL_EXECUTION_TOOLS.contains(&tool_name) {
        return None;
    }
    let command = tool_args.get("command").and_then(Value::as_str)?;
    check_shell_command_safety(command)
}

#[must_use]
pub fn check_shell_command_safety(command: &str) -> Option<String> {
    if command.contains("${!") {
        return Some(
            "shell command contains `${!...}` indirect expansion, which can construct commands dynamically"
                .to_string(),
        );
    }
    if command.contains("@P}") {
        return Some(
            "shell command contains `${...@P}` parameter expansion, which can hide the real command"
                .to_string(),
        );
    }
    if shell_command_uses_dynamic_eval(command) {
        return Some(
            "shell command uses `eval` with shell expansion, which can reconstruct commands dynamically"
                .to_string(),
        );
    }
    None
}

fn shell_command_uses_dynamic_eval(command: &str) -> bool {
    let has_eval = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '\n'))
        .any(|token| token.eq_ignore_ascii_case("eval"));
    has_eval && (command.contains("$(") || command.contains("${") || command.contains('$'))
}

#[must_use]
pub fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push(' ');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut depth = 1u32;
            while depth > 0 {
                match chars.next() {
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        depth -= 1;
                    }
                    None => break,
                    _ => {}
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

#[must_use]
pub fn check_sql_safety(sql: &str) -> Option<&'static str> {
    let stripped = strip_sql_comments(sql).to_uppercase();
    for stmt in stripped.split(';') {
        let first_word = stmt.split_whitespace().next().unwrap_or("");
        for &kw in DESTRUCTIVE_KEYWORDS {
            if first_word == kw {
                return Some(kw);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sql_safety_blocks_commented_multi_statement() {
        assert_eq!(
            check_sql_safety("-- safe\nSELECT 1; /* comment */ ALTER TABLE t ADD c INT"),
            Some("ALTER")
        );
        assert_eq!(check_sql_safety("SELECT 1; DROP TABLE users"), Some("DROP"));
    }

    #[test]
    fn middleware_blocks_destructive_mo_query_without_opt_in() {
        let decision =
            evaluate_tool_safety_request("mo_query", &json!({"sql": "DROP TABLE users"}));
        assert!(matches!(
            decision,
            SafetyMiddlewareDecision::Deny(reason)
                if reason.contains("destructive_sql") && reason.contains("allow_destructive")
        ));
    }

    #[test]
    fn middleware_allows_destructive_mo_query_with_opt_in() {
        let decision = evaluate_tool_safety_request(
            "mo_query",
            &json!({"sql": "DROP TABLE users", "allow_destructive": true}),
        );
        assert_eq!(decision, SafetyMiddlewareDecision::Allow);
    }

    #[test]
    fn middleware_blocks_shell_var_at_p_expansion() {
        let decision =
            evaluate_tool_safety_request("bash", &json!({"command": "printf %s ${payload@P}"}));
        assert!(matches!(
            decision,
            SafetyMiddlewareDecision::Deny(reason)
                if reason.contains("shell_obfuscation") && reason.contains("@P")
        ));
    }

    #[test]
    fn middleware_blocks_shell_indirect_expansion() {
        let decision =
            evaluate_tool_safety_request("bash", &json!({"command": "echo ${!payload}"}));
        assert!(matches!(
            decision,
            SafetyMiddlewareDecision::Deny(reason)
                if reason.contains("shell_obfuscation") && reason.contains("indirect expansion")
        ));
    }

    #[test]
    fn middleware_blocks_eval_with_expansion() {
        let decision =
            evaluate_tool_safety_request("bash", &json!({"command": "eval \"$PAYLOAD\""}));
        assert!(matches!(
            decision,
            SafetyMiddlewareDecision::Deny(reason)
                if reason.contains("shell_obfuscation") && reason.contains("eval")
        ));
    }

    #[test]
    fn middleware_allows_plain_shell_command() {
        let decision = evaluate_tool_safety_request("bash", &json!({"command": "git status"}));
        assert_eq!(decision, SafetyMiddlewareDecision::Allow);
    }
}
