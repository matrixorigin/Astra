/// Parsed response from the unified /compact prompt.
///
/// Replaces the previous 3-call pipeline (summary → fact extraction → synthesis)
/// with a single structured JSON response. This reduces latency from 15-30s to 5-10s
/// and simplifies the code path.
/// Unified prompt for /compact that generates summary + extracts facts in one LLM call.
pub const COMPACT_UNIFIED_PROMPT: &str = r##"
Summarize this conversation and extract structured facts in ONE response.

## Output format
Return a JSON object with two fields:

{
  "summary": {
    "goals": ["bullet 1", "bullet 2"],
    "decisions": ["bullet 1"],
    "actions": ["bullet 1"],
    "status": ["bullet 1"],
    "key_facts": ["bullet 1"]
  },
  "facts": [
    {"fact": "self-contained sentence ≤30 words", "type": "semantic"}
  ]
}

## Rules for summary
- <250 words total across all 5 sections
- Bullets only, no prose
- Each section must have at least one bullet (or empty array if truly nothing)

## Rules for facts
- 3-8 facts maximum (fewer is better if summary is short)
- Each fact must stand alone without the surrounding conversation
- Prefer: user preferences, project conventions, decisions, recurring patterns
- DO NOT extract: transient errors, file contents, raw tool output, one-off commands
- Types: "semantic" (general knowledge/preference), "profile" (user info), "procedural" (how-to / convention), "working" (transient project state)
- If no facts are worth remembering, return empty array: []
"##;

/// Parsed response from the unified /compact prompt.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompactResponse {
    pub summary: CompactSummary,
    #[serde(default)]
    pub facts: Vec<CompactFact>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompactSummary {
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default)]
    pub key_facts: Vec<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CompactFact {
    pub fact: String,
    #[serde(rename = "type", default = "default_fact_type")]
    pub fact_type: String,
}

fn default_fact_type() -> String {
    "semantic".to_string()
}

impl CompactResponse {
    /// Render the summary section as human-readable markdown.
    pub fn render_summary(&self) -> String {
        let mut lines = Vec::new();
        if !self.summary.goals.is_empty() {
            lines.push("### Goals".to_string());
            for b in &self.summary.goals {
                lines.push(format!("- {b}"));
            }
        }
        if !self.summary.decisions.is_empty() {
            lines.push("### Decisions".to_string());
            for b in &self.summary.decisions {
                lines.push(format!("- {b}"));
            }
        }
        if !self.summary.actions.is_empty() {
            lines.push("### Actions".to_string());
            for b in &self.summary.actions {
                lines.push(format!("- {b}"));
            }
        }
        if !self.summary.status.is_empty() {
            lines.push("### Status".to_string());
            for b in &self.summary.status {
                lines.push(format!("- {b}"));
            }
        }
        if !self.summary.key_facts.is_empty() {
            lines.push("### Key Facts".to_string());
            for b in &self.summary.key_facts {
                lines.push(format!("- {b}"));
            }
        }
        lines.join("\n")
    }

    /// Extract valid facts, logging unknown types instead of silently coercing.
    pub fn valid_facts(&self) -> Vec<(String, String)> {
        self.facts
            .iter()
            .filter_map(|f| {
                let fact = f.fact.trim();
                if fact.is_empty() {
                    return None;
                }
                match f.fact_type.as_str() {
                    "semantic" | "profile" | "procedural" | "working" => {
                        Some((fact.to_string(), f.fact_type.clone()))
                    }
                    unknown => {
                        eprintln!(
                            "[compact] Unknown fact_type={unknown:?}, discarding fact={fact:?}"
                        );
                        None
                    }
                }
            })
            .collect()
    }
}

/// Parse the unified /compact response. Tolerates markdown fences and extra whitespace.
///
/// Uses bracket matching (not greedy `rfind`) to correctly extract the JSON object
/// even when the LLM output contains embedded JSON fragments or prose with braces.
/// Tries each `{` starting position until a valid CompactResponse is parsed.
pub fn parse_compact_response(raw: &str) -> Option<CompactResponse> {
    let trimmed = raw.trim();
    let json_str = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };

    // Try direct parse
    if let Ok(resp) = serde_json::from_str::<CompactResponse>(json_str) {
        return Some(resp);
    }

    // Bracket-matching extraction: try each '{' and find its matching '}'
    let bytes = json_str.as_bytes();
    let mut search_start = 0usize;
    while let Some(start) = json_str[search_start..].find('{') {
        let abs_start = search_start + start;
        let mut depth = 0u32;
        let mut end = abs_start;
        let mut in_string = false;
        let mut escaped = false;
        for (i, &b) in bytes.iter().enumerate().skip(abs_start) {
            if escaped {
                escaped = false;
                continue;
            }
            if b == b'\\' && in_string {
                escaped = true;
                continue;
            }
            if b == b'"' {
                in_string = !in_string;
                continue;
            }
            if in_string {
                continue;
            }
            if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
        }
        let slice = &json_str[abs_start..=end];
        if let Ok(resp) = serde_json::from_str::<CompactResponse>(slice) {
            return Some(resp);
        }
        search_start = abs_start + 1;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compact_response_direct_json() {
        let input = r#"{"summary":{"goals":["finish feature"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).unwrap();
        assert_eq!(resp.summary.goals, vec!["finish feature"]);
        assert!(resp.facts.is_empty());
    }

    #[test]
    fn parse_compact_response_with_facts() {
        let input = r#"{"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"Uses Rust","type":"procedural"}]}"#;
        let resp = parse_compact_response(input).unwrap();
        assert_eq!(resp.facts.len(), 1);
        assert_eq!(resp.facts[0].fact, "Uses Rust");
        assert_eq!(resp.facts[0].fact_type, "procedural");
    }

    #[test]
    fn parse_compact_response_with_markdown_fences() {
        let input = "```json\n{\"summary\":{\"goals\":[],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}\n```";
        let resp = parse_compact_response(input).unwrap();
        assert!(resp.facts.is_empty());
    }

    #[test]
    fn parse_compact_response_with_plain_fences() {
        let input = "```\n{\"summary\":{\"goals\":[],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}\n```";
        let resp = parse_compact_response(input).unwrap();
        assert!(resp.facts.is_empty());
    }

    #[test]
    fn parse_compact_response_with_preamble() {
        let input = "Here is the compact output:\n{\"summary\":{\"goals\":[\"fix bug\"],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}";
        let resp = parse_compact_response(input).unwrap();
        assert_eq!(resp.summary.goals, vec!["fix bug"]);
    }

    #[test]
    fn parse_compact_response_bracket_matching_not_greedy() {
        // The response contains an embedded JSON fragment in prose before the real output.
        // Greedy rfind('}') would match the wrong closing brace.
        let input = r#"The output is {"nested": "value"} and here is the real result: {"summary":{"goals":["correct"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).unwrap();
        assert_eq!(resp.summary.goals, vec!["correct"]);
    }

    #[test]
    fn parse_compact_response_bracket_matching_deeply_nested() {
        let input = r#"Preamble {"a":{"b":{"c":1}}} and result: {"summary":{"goals":["nested ok"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"deep","type":"semantic"}]} trailing"#;
        let resp = parse_compact_response(input).unwrap();
        assert_eq!(resp.summary.goals, vec!["nested ok"]);
        assert_eq!(resp.facts[0].fact, "deep");
    }

    #[test]
    fn parse_compact_response_garbage_returns_none() {
        assert!(parse_compact_response("not json at all").is_none());
        assert!(parse_compact_response("").is_none());
    }

    #[test]
    fn parse_compact_response_no_braces() {
        assert!(parse_compact_response("just some text without braces").is_none());
    }

    #[test]
    fn valid_facts_rejects_unknown_types() {
        let resp = CompactResponse {
            summary: CompactSummary {
                goals: vec![],
                decisions: vec![],
                actions: vec![],
                status: vec![],
                key_facts: vec![],
            },
            facts: vec![
                CompactFact {
                    fact: "a".into(),
                    fact_type: "semantic".into(),
                },
                CompactFact {
                    fact: "b".into(),
                    fact_type: "invalid".into(),
                },
            ],
        };
        let facts = resp.valid_facts();
        // Only "a" should be kept; "b" with invalid type should be dropped
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "a");
    }

    #[test]
    fn valid_facts_accepts_all_valid_types() {
        for t in &["semantic", "profile", "procedural", "working"] {
            let resp = CompactResponse {
                summary: CompactSummary {
                    goals: vec![],
                    decisions: vec![],
                    actions: vec![],
                    status: vec![],
                    key_facts: vec![],
                },
                facts: vec![CompactFact {
                    fact: "test".into(),
                    fact_type: t.to_string(),
                }],
            };
            let facts = resp.valid_facts();
            assert_eq!(facts.len(), 1, "failed for type: {t}");
            assert_eq!(facts[0].1, *t);
        }
    }

    #[test]
    fn valid_facts_filters_empty() {
        let resp = CompactResponse {
            summary: CompactSummary {
                goals: vec![],
                decisions: vec![],
                actions: vec![],
                status: vec![],
                key_facts: vec![],
            },
            facts: vec![CompactFact {
                fact: "".into(),
                fact_type: "semantic".into(),
            }],
        };
        assert!(resp.valid_facts().is_empty());
    }

    #[test]
    fn render_summary_all_sections() {
        let resp = CompactResponse {
            summary: CompactSummary {
                goals: vec!["g1".into()],
                decisions: vec!["d1".into()],
                actions: vec!["a1".into()],
                status: vec!["s1".into()],
                key_facts: vec!["k1".into()],
            },
            facts: vec![],
        };
        let rendered = resp.render_summary();
        assert!(rendered.contains("### Goals"));
        assert!(rendered.contains("- g1"));
        assert!(rendered.contains("### Decisions"));
        assert!(rendered.contains("### Actions"));
        assert!(rendered.contains("### Status"));
        assert!(rendered.contains("### Key Facts"));
    }

    #[test]
    fn render_summary_empty_sections_omitted() {
        let resp = CompactResponse {
            summary: CompactSummary {
                goals: vec!["only goal".into()],
                decisions: vec![],
                actions: vec![],
                status: vec![],
                key_facts: vec![],
            },
            facts: vec![],
        };
        let rendered = resp.render_summary();
        assert!(rendered.contains("### Goals"));
        assert!(!rendered.contains("### Decisions"));
    }
}
