use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_segmentation::UnicodeSegmentation;

/// Maximum text copied from one delegated result into session-state
/// projections. The child transcript remains the canonical full-fidelity
/// record; this payload is only navigation evidence for ancestors.
pub const MAX_DELEGATED_FINDING_SUMMARY_CHARS: usize = 1_000;
pub const MAX_DELEGATED_FINDING_OUTPUT_BYTES: usize = 256 * 1024;
pub const MAX_DELEGATED_FINDINGS: usize = 32;
pub const MAX_DELEGATED_FINDING_EVIDENCE_ITEMS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegatedFindingSeverity {
    Critical,
    High,
    Medium,
    Low,
    Info,
    Unknown,
}

impl DelegatedFindingSeverity {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
            Self::Info => "info",
            Self::Unknown => "unknown",
        }
    }
}

impl Serialize for DelegatedFindingSeverity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DelegatedFindingSeverity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "critical" => Self::Critical,
            "high" => Self::High,
            "medium" => Self::Medium,
            "low" => Self::Low,
            "info" => Self::Info,
            _ => Self::Unknown,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedFinding {
    pub severity: DelegatedFindingSeverity,
    pub summary: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegatedFindingEnvelope {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<DelegatedFinding>,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub verdict: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DelegatedFindingParse {
    Structured(DelegatedFindingEnvelope),
    /// Deployment-window migration for the exact review labels emitted by
    /// the previous system prompt. This is deliberately not keyword matching.
    LegacyReview(DelegatedFindingEnvelope),
    Unstructured,
    MalformedJson(String),
    ResourceLimitExceeded(String),
}

impl DelegatedFindingEnvelope {
    pub fn parse(output: &str) -> DelegatedFindingParse {
        let trimmed = output.trim();
        if trimmed.len() > MAX_DELEGATED_FINDING_OUTPUT_BYTES {
            return DelegatedFindingParse::ResourceLimitExceeded(format!(
                "output is {} bytes; limit is {} bytes",
                trimmed.len(),
                MAX_DELEGATED_FINDING_OUTPUT_BYTES
            ));
        }
        if trimmed.starts_with('{') {
            return match serde_json::from_str(trimmed) {
                Ok(envelope) => match validate_envelope_shape(&envelope) {
                    Ok(()) => DelegatedFindingParse::Structured(envelope),
                    Err(error) => DelegatedFindingParse::ResourceLimitExceeded(error),
                },
                Err(error) => DelegatedFindingParse::MalformedJson(error.to_string()),
            };
        }

        let findings = trimmed
            .lines()
            .filter_map(parse_legacy_review_line)
            .take(MAX_DELEGATED_FINDINGS + 1)
            .collect::<Vec<_>>();
        if findings.is_empty() {
            DelegatedFindingParse::Unstructured
        } else {
            let envelope = Self {
                findings,
                ..Self::default()
            };
            match validate_envelope_shape(&envelope) {
                Ok(()) => DelegatedFindingParse::LegacyReview(envelope),
                Err(error) => DelegatedFindingParse::ResourceLimitExceeded(error),
            }
        }
    }

    pub fn critical_summary(self) -> Option<String> {
        let summaries = self
            .findings
            .into_iter()
            .filter(|finding| finding.severity == DelegatedFindingSeverity::Critical)
            .filter_map(|finding| {
                let summary = finding.summary.trim();
                if summary.is_empty() {
                    return None;
                }
                let evidence = finding
                    .evidence
                    .into_iter()
                    .map(|item| item.trim().to_string())
                    .filter(|item| !item.is_empty())
                    .collect::<Vec<_>>();
                Some(if evidence.is_empty() {
                    summary.to_string()
                } else {
                    format!("{summary}\nEvidence:\n- {}", evidence.join("\n- "))
                })
            })
            .collect::<Vec<_>>();
        if summaries.is_empty() {
            return None;
        }
        Some(truncate_chars(
            &summaries.join("\n\n"),
            MAX_DELEGATED_FINDING_SUMMARY_CHARS,
        ))
    }
}

fn parse_legacy_review_line(line: &str) -> Option<DelegatedFinding> {
    let line = line.trim().trim_start_matches(['-', '*']).trim_start();
    let (label, summary) = line.split_once(':')?;
    let severity = match label.trim().to_ascii_lowercase().as_str() {
        "must-fix" => DelegatedFindingSeverity::Critical,
        "should-fix" => DelegatedFindingSeverity::High,
        "suggestion" => DelegatedFindingSeverity::Low,
        _ => return None,
    };
    let summary = summary.trim();
    (!summary.is_empty()).then(|| DelegatedFinding {
        severity,
        summary: summary.to_string(),
        evidence: Vec::new(),
    })
}

pub fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut used: usize = 0;
    value
        .graphemes(true)
        .take_while(|grapheme| {
            let chars = grapheme.chars().count();
            if used.saturating_add(chars) > max_chars {
                return false;
            }
            used += chars;
            true
        })
        .collect()
}

fn validate_envelope_shape(envelope: &DelegatedFindingEnvelope) -> Result<(), String> {
    if envelope.findings.len() > MAX_DELEGATED_FINDINGS {
        return Err(format!(
            "envelope contains {} findings; limit is {}",
            envelope.findings.len(),
            MAX_DELEGATED_FINDINGS
        ));
    }
    if let Some((index, finding)) = envelope
        .findings
        .iter()
        .enumerate()
        .find(|(_, finding)| finding.evidence.len() > MAX_DELEGATED_FINDING_EVIDENCE_ITEMS)
    {
        return Err(format!(
            "finding {index} contains {} evidence items; limit is {}",
            finding.evidence.len(),
            MAX_DELEGATED_FINDING_EVIDENCE_ITEMS
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_is_case_insensitive_and_unknown_is_non_fatal() {
        let parsed = DelegatedFindingEnvelope::parse(
            r#"{"findings":[{"severity":"CRITICAL","summary":"a"},{"severity":"future","summary":"b"}]}"#,
        );
        let DelegatedFindingParse::Structured(envelope) = parsed else {
            panic!("expected structured envelope");
        };
        assert_eq!(
            envelope.findings[0].severity,
            DelegatedFindingSeverity::Critical
        );
        assert_eq!(
            envelope.findings[1].severity,
            DelegatedFindingSeverity::Unknown
        );
        assert_eq!(envelope.critical_summary().as_deref(), Some("a"));
    }

    #[test]
    fn known_legacy_review_labels_have_a_bounded_migration_path() {
        let parsed = DelegatedFindingEnvelope::parse(
            "- must-fix: tenant boundary bypass\n- should-fix: add coverage",
        );
        let DelegatedFindingParse::LegacyReview(envelope) = parsed else {
            panic!("expected legacy review envelope");
        };
        assert_eq!(
            envelope.critical_summary().as_deref(),
            Some("tenant boundary bypass")
        );
        assert!(matches!(
            DelegatedFindingEnvelope::parse("a critical prose sentence"),
            DelegatedFindingParse::Unstructured
        ));
    }

    #[test]
    fn critical_summary_joins_findings_and_truncates_on_unicode_boundaries() {
        let long = "界".repeat(MAX_DELEGATED_FINDING_SUMMARY_CHARS);
        let envelope = DelegatedFindingEnvelope {
            findings: vec![
                DelegatedFinding {
                    severity: DelegatedFindingSeverity::Critical,
                    summary: "first".into(),
                    evidence: vec!["a.rs:1".into()],
                },
                DelegatedFinding {
                    severity: DelegatedFindingSeverity::Critical,
                    summary: long,
                    evidence: Vec::new(),
                },
            ],
            ..DelegatedFindingEnvelope::default()
        };

        let summary = envelope.critical_summary().expect("critical summary");
        assert!(summary.starts_with("first\nEvidence:\n- a.rs:1\n\n"));
        assert_eq!(summary.chars().count(), MAX_DELEGATED_FINDING_SUMMARY_CHARS);
        assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
    }

    #[test]
    fn malformed_json_is_not_confused_with_an_empty_result() {
        assert!(matches!(
            DelegatedFindingEnvelope::parse(r#"{"findings": ["#),
            DelegatedFindingParse::MalformedJson(_)
        ));
        assert!(matches!(
            DelegatedFindingEnvelope::parse(r#"{"findings":[]}"#),
            DelegatedFindingParse::Structured(_)
        ));
    }

    #[test]
    fn untrusted_output_has_allocation_and_collection_limits() {
        let oversized = "x".repeat(MAX_DELEGATED_FINDING_OUTPUT_BYTES + 1);
        assert!(matches!(
            DelegatedFindingEnvelope::parse(&oversized),
            DelegatedFindingParse::ResourceLimitExceeded(_)
        ));

        let findings = (0..=MAX_DELEGATED_FINDINGS)
            .map(|idx| serde_json::json!({"severity":"info","summary":idx.to_string()}))
            .collect::<Vec<_>>();
        assert!(matches!(
            DelegatedFindingEnvelope::parse(&serde_json::json!({"findings":findings}).to_string()),
            DelegatedFindingParse::ResourceLimitExceeded(_)
        ));
    }

    #[test]
    fn truncation_preserves_grapheme_clusters() {
        let family = "👨‍👩‍👧‍👦";
        let value = format!("a{family}b");
        assert_eq!(truncate_chars(&value, 2), "a");
        assert_eq!(truncate_chars(&value, 8), format!("a{family}"));
    }
}
