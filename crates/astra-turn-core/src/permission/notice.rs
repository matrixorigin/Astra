//! Structured permission notices that may cross text-only terminal adapters.

pub const AUTO_APPROVED_PERMISSION_PREFIX: &str = "Auto-approved tool permission:";

pub fn format_auto_approved_permission(tool: &str, reason: &str) -> String {
    format!("  🔓 {AUTO_APPROVED_PERMISSION_PREFIX} {tool} ({reason})")
}

pub fn parse_auto_approved_permission(line: &str) -> Option<(String, String)> {
    let body = line
        .trim()
        .strip_prefix('🔓')
        .map(str::trim)
        .unwrap_or_else(|| line.trim())
        .strip_prefix(AUTO_APPROVED_PERMISSION_PREFIX)?
        .trim();
    let (tool, reason) = body.rsplit_once(" (")?;
    Some((
        tool.trim().to_string(),
        reason.strip_suffix(')').unwrap_or(reason).to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_approved_permission_round_trips() {
        let line = format_auto_approved_permission("bash", "agent policy allowlist");
        assert_eq!(
            parse_auto_approved_permission(&line),
            Some(("bash".to_string(), "agent policy allowlist".to_string()))
        );
    }

    #[test]
    fn auto_approved_permission_round_trips_tool_names_with_parens() {
        let line = format_auto_approved_permission("tool (alpha)", "agent policy allowlist");
        assert_eq!(
            parse_auto_approved_permission(&line),
            Some((
                "tool (alpha)".to_string(),
                "agent policy allowlist".to_string()
            ))
        );
    }
}
