//! Shared labels used by the text and HTML Explain Analyze renderers.

use astra_turn_types::ExplainAnalyzeProjectionDiagnosticCodeV1;

pub(crate) fn alpha_label(mut ordinal: usize) -> String {
    let mut label = String::new();
    loop {
        label.insert(
            0,
            char::from(b'A' + u8::try_from(ordinal % 26).unwrap_or(0)),
        );
        ordinal /= 26;
        if ordinal == 0 {
            return label;
        }
        ordinal -= 1;
    }
}

pub(crate) fn diagnostic_label(code: ExplainAnalyzeProjectionDiagnosticCodeV1) -> &'static str {
    use ExplainAnalyzeProjectionDiagnosticCodeV1::*;
    match code {
        ConflictingFact => "conflicting runtime facts",
        DependencyCycle => "cyclic dependency",
        InvalidEvent => "invalid runtime fact",
        MissingDependency => "dependency was not observed",
        MissingParent => "parent stage was not observed",
        ParentCycle => "cyclic stage hierarchy",
        UnresolvedTerminalNode => "stage did not reach a recorded end",
    }
}

pub(crate) fn format_ms(ms: u64) -> String {
    if ms >= 1_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{ms}ms")
    }
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else if tokens >= 1_000 {
        format!("{:.2}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}
