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
///
/// When multiple valid `{...}` blocks are present, prefers the **richest** one
/// (most non-empty summary fields and facts). LLMs frequently emit a stub or
/// example object before the real answer; selecting by content prevents the
/// stub from winning and producing an empty `/compact` summary.
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

    // Try direct parse first; if it has any content, use it.
    if let Ok(resp) = serde_json::from_str::<CompactResponse>(json_str)
        && response_score(&resp) > 0
    {
        return Some(resp);
    }

    // Bracket-matching extraction: enumerate ALL valid CompactResponse blocks,
    // then return the one with the highest score (most filled fields).
    let bytes = json_str.as_bytes();
    let mut search_start = 0usize;
    let mut best: Option<(usize, CompactResponse)> = None;
    while let Some(start) = json_str[search_start..].find('{') {
        let abs_start = search_start + start;
        let mut depth = 0u32;
        let mut end = None;
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
                depth = depth.saturating_add(1);
            } else if b == b'}' {
                if depth == 0 {
                    // Stray closing brace before any open — abort this start.
                    break;
                }
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
        }
        if let Some(end) = end {
            let slice = &json_str[abs_start..=end];
            if let Ok(resp) = serde_json::from_str::<CompactResponse>(slice) {
                let score = response_score(&resp);
                // Prefer the richest match. On ties (e.g. both empty),
                // prefer the LATER one — the LLM almost always emits the
                // real answer after any stub/example.
                let take = match &best {
                    None => true,
                    Some((best_score, _)) => score >= *best_score,
                };
                if take {
                    best = Some((score, resp));
                }
            }
        }
        search_start = abs_start + 1;
    }

    best.map(|(_, resp)| resp).or_else(|| {
        // If nothing matched the score-filter, fall back to a direct-parse
        // result (even if empty) — better an empty summary than nothing.
        serde_json::from_str::<CompactResponse>(json_str).ok()
    })
}

/// Score a parsed CompactResponse by how much real content it carries.
/// Used to disambiguate when multiple valid JSON blocks are present.
fn response_score(r: &CompactResponse) -> usize {
    r.summary.goals.iter().filter(|s| !s.is_empty()).count()
        + r.summary.decisions.iter().filter(|s| !s.is_empty()).count()
        + r.summary.actions.iter().filter(|s| !s.is_empty()).count()
        + r.summary.status.iter().filter(|s| !s.is_empty()).count()
        + r.summary.key_facts.iter().filter(|s| !s.is_empty()).count()
        + r.facts.iter().filter(|f| !f.fact.is_empty()).count()
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
    fn parse_compact_response_prefers_richer_match_over_empty_stub() {
        // Regression: the LLM emits an empty schema-shaped stub before
        // the real answer. The parser must NOT lock in on the empty stub.
        let input = r#"Example output: {"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}
Real answer: {"summary":{"goals":["finish refactor"],"decisions":["use typed pipeline"],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"branch is fix_0602_03","type":"semantic"}]}"#;
        let resp = parse_compact_response(input).expect("must parse the rich answer");
        assert_eq!(resp.summary.goals, vec!["finish refactor"]);
        assert_eq!(resp.summary.decisions, vec!["use typed pipeline"]);
        assert_eq!(resp.facts.len(), 1);
        assert_eq!(resp.facts[0].fact, "branch is fix_0602_03");
    }

    #[test]
    fn parse_compact_response_prefers_later_when_scores_tie() {
        // Two equally-rich answers: prefer the later one. LLMs emit
        // their final answer last; an earlier draft should not win.
        let input = r#"Draft: {"summary":{"goals":["draft goal"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}
Final: {"summary":{"goals":["final goal"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("must parse");
        assert_eq!(
            resp.summary.goals,
            vec!["final goal"],
            "tie-break must pick the later block"
        );
    }

    #[test]
    fn parse_compact_response_only_stub_returns_stub() {
        // Single empty stub is still parseable — return it (caller decides).
        let input = r#"{"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("must parse");
        assert!(resp.summary.goals.is_empty());
        assert!(resp.facts.is_empty());
    }

    #[test]
    fn parse_compact_response_handles_stray_closing_brace_without_panic() {
        // Stray `}` before any `{` must not panic via depth underflow.
        let input = r#"} prose then {"summary":{"goals":["ok"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("should still find the real block");
        assert_eq!(resp.summary.goals, vec!["ok"]);
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
