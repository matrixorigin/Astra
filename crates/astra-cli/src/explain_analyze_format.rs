//! Shared presentation of recorded Explain Analyze facts.

use astra_turn_types::{
    ExplainAnalyzeDecisionDetailV1, ExplainAnalyzeEventV1, ExplainAnalyzeGraphV1,
    ExplainAnalyzeProjectionDiagnosticCodeV1, ExplainAnalyzeUsageBasisV1,
};

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

pub(crate) fn decision_detail_line(detail: &ExplainAnalyzeDecisionDetailV1) -> String {
    match detail {
        ExplainAnalyzeDecisionDetailV1::DelegationCatalogResolution {
            requirement_index,
            match_count,
        } if *match_count == 0 => format!(
            "Requested model requirement {}: no eligible model matched in the authorized catalog snapshot; no child was started.",
            requirement_index.saturating_add(1)
        ),
        ExplainAnalyzeDecisionDetailV1::DelegationCatalogResolution {
            requirement_index,
            match_count,
        } => format!(
            "Requested model requirement {}: {match_count} eligible models matched; selection was ambiguous, so no child was started.",
            requirement_index.saturating_add(1)
        ),
        ExplainAnalyzeDecisionDetailV1::Failure { error_kind } => {
            format!("Error · {}", error_kind.as_str().replace('_', " "))
        }
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

pub(crate) fn project(events: &[ExplainAnalyzeEventV1]) -> ExplainAnalyzeGraphV1 {
    let mut graph = ExplainAnalyzeGraphV1::default();
    for event in events {
        graph.apply(event.clone());
    }
    graph.finish_ingest();
    graph
}

pub(crate) fn compact_usage(usage: &astra_turn_types::ExplainAnalyzeTokenUsageV1) -> String {
    let mut lanes = Vec::new();
    if let Some(value) = usage.fresh_input_tokens {
        lanes.push(format!("in {}", format_tokens(value)));
    }
    if let Some(value) = usage.output_tokens {
        lanes.push(format!("out {}", format_tokens(value)));
    }
    let value = if lanes.is_empty() {
        if usage.cache_read_tokens.is_some() || usage.cache_creation_tokens.is_some() {
            "cache".to_string()
        } else {
            "reported".to_string()
        }
    } else {
        lanes.join(" / ")
    };
    match usage.basis {
        ExplainAnalyzeUsageBasisV1::ProviderExact => value,
        ExplainAnalyzeUsageBasisV1::ProviderPartial => format!("partial {value}"),
        ExplainAnalyzeUsageBasisV1::RuntimeEstimated => format!("~{value}"),
    }
}

pub(crate) fn provider_usage_summary(graph: &ExplainAnalyzeGraphV1) -> Option<String> {
    let usage = graph.observed_provider_usage();
    if usage.attempts == 0 {
        return None;
    }
    let values = [
        ("in", usage.fresh_input),
        ("cache read", usage.cache_read),
        ("cache write", usage.cache_creation),
        ("out", usage.output),
    ]
    .into_iter()
    .filter(|(_, lane)| lane.reporting_attempts > 0)
    .map(|(label, lane)| {
        let coverage = if lane.reporting_attempts == usage.attempts {
            String::new()
        } else {
            format!(" {}/{}", lane.reporting_attempts, usage.attempts)
        };
        format!("{label} {}{coverage}", format_tokens_u128(lane.tokens))
    })
    .collect::<Vec<_>>();
    if values.is_empty() {
        return Some("Provider tokens · usage not reported yet".to_string());
    }
    Some(format!("Provider tokens · {}", values.join(" · ")))
}

pub(crate) fn format_tokens_u128(value: u128) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}
