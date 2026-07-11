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
- 0-5 facts maximum; empty is preferable to a weak inference
- Each fact must stand alone without the surrounding conversation
- Extract only explicit durable user preferences/profile facts or stable project conventions that were confirmed or repeated
- A requirement in the current request is not automatically a durable preference; do not rewrite it as "the user prefers..."
- Keep current-task constraints, branch/repository state, one-off decisions, and pending work in the summary, not facts
- DO NOT extract: transient errors, file contents, raw tool output, one-off commands
- Types: "semantic" (stable knowledge), "profile" (explicit user info/preference), "procedural" (stable convention)
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
                    "semantic" | "profile" | "procedural" => {
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

    // ── parse_compact_response ──

    #[test]
    fn parse_success_variants() {
        #[allow(clippy::type_complexity)]
        let cases: Vec<(&str, &[&str], &[(&str, &str)])> = vec![
            // (input, expected_goals, expected_facts)
            (
                r#"{"summary":{"goals":["finish feature"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#,
                &["finish feature"][..],
                &[],
            ),
            (
                r#"{"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"Uses Rust","type":"procedural"}]}"#,
                &[],
                &[("Uses Rust", "procedural")],
            ),
            (
                "```json\n{\"summary\":{\"goals\":[],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}\n```",
                &[],
                &[],
            ),
            (
                "```\n{\"summary\":{\"goals\":[],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}\n```",
                &[],
                &[],
            ),
            (
                "Here is the compact output:\n{\"summary\":{\"goals\":[\"fix bug\"],\"decisions\":[],\"actions\":[],\"status\":[],\"key_facts\":[]},\"facts\":[]}",
                &["fix bug"][..],
                &[],
            ),
        ];
        for (input, expected_goals, expected_facts) in cases {
            let resp = parse_compact_response(input).unwrap();
            assert_eq!(resp.summary.goals, expected_goals, "goals mismatch");
            assert_eq!(resp.facts.len(), expected_facts.len(), "facts len mismatch");
            for (i, (fact, ftype)) in expected_facts.iter().enumerate() {
                assert_eq!(resp.facts[i].fact, *fact);
                assert_eq!(resp.facts[i].fact_type, *ftype);
            }
        }
    }

    #[test]
    fn parse_bracket_matching_edge_cases() {
        let cases: Vec<(&str, &str)> = vec![
            // (input, expected_first_goal)
            (
                r#"The output is {"nested": "value"} and here is the real result: {"summary":{"goals":["correct"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#,
                "correct",
            ),
            (
                r#"Preamble {"a":{"b":{"c":1}}} and result: {"summary":{"goals":["nested ok"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"deep","type":"semantic"}]} trailing"#,
                "nested ok",
            ),
        ];
        for (input, expected_goal) in cases {
            let resp = parse_compact_response(input).unwrap();
            assert_eq!(resp.summary.goals, vec![expected_goal]);
        }
    }

    #[test]
    fn parse_none_and_error_cases() {
        let none_cases = ["not json at all", "", "just some text without braces"];
        for input in none_cases {
            assert!(
                parse_compact_response(input).is_none(),
                "expected None for: {input}"
            );
        }

        // Stray closing brace must not panic
        let input = r#"} prose then {"summary":{"goals":["ok"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("should still find the real block");
        assert_eq!(resp.summary.goals, vec!["ok"]);
    }

    #[test]
    fn parse_prefers_richer_or_later_answer() {
        // Prefers richer match over empty stub
        let input = r#"Example output: {"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}
Real answer: {"summary":{"goals":["finish refactor"],"decisions":["use typed pipeline"],"actions":[],"status":[],"key_facts":[]},"facts":[{"fact":"branch is fix_0602_03","type":"semantic"}]}"#;
        let resp = parse_compact_response(input).expect("must parse the rich answer");
        assert_eq!(resp.summary.goals, vec!["finish refactor"]);
        assert_eq!(resp.facts[0].fact, "branch is fix_0602_03");

        // Tie-break: when scores equal, prefer later
        let input = r#"Draft: {"summary":{"goals":["draft goal"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}
Final: {"summary":{"goals":["final goal"],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("must parse");
        assert_eq!(resp.summary.goals, vec!["final goal"]);

        // Single empty stub is parseable
        let input = r#"{"summary":{"goals":[],"decisions":[],"actions":[],"status":[],"key_facts":[]},"facts":[]}"#;
        let resp = parse_compact_response(input).expect("must parse");
        assert!(resp.summary.goals.is_empty());
        assert!(resp.facts.is_empty());
    }

    // ── valid_facts ──

    #[test]
    fn valid_facts_filtering() {
        let base = || CompactSummary {
            goals: vec![],
            decisions: vec![],
            actions: vec![],
            status: vec![],
            key_facts: vec![],
        };

        // Rejects unknown types
        let resp = CompactResponse {
            summary: base(),
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
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "a");

        // Accepts only durable fact types. Working state belongs to the
        // session-memory snapshot, not cross-session recall.
        for t in &["semantic", "profile", "procedural"] {
            let resp = CompactResponse {
                summary: base(),
                facts: vec![CompactFact {
                    fact: "test".into(),
                    fact_type: t.to_string(),
                }],
            };
            let facts = resp.valid_facts();
            assert_eq!(facts.len(), 1, "failed for type: {t}");
            assert_eq!(facts[0].1, *t);
        }
        let working = CompactResponse {
            summary: base(),
            facts: vec![CompactFact {
                fact: "current branch is dirty".into(),
                fact_type: "working".into(),
            }],
        };
        assert!(working.valid_facts().is_empty());

        // Filters empty facts
        let resp = CompactResponse {
            summary: base(),
            facts: vec![CompactFact {
                fact: "".into(),
                fact_type: "semantic".into(),
            }],
        };
        assert!(resp.valid_facts().is_empty());
    }

    // ── render_summary ──

    #[test]
    fn render_summary_output() {
        // All sections populated
        let all = CompactSummary {
            goals: vec!["g1".into()],
            decisions: vec!["d1".into()],
            actions: vec!["a1".into()],
            status: vec!["s1".into()],
            key_facts: vec!["k1".into()],
        };
        let rendered = CompactResponse {
            summary: all,
            facts: vec![],
        }
        .render_summary();
        for section in &["Goals", "Decisions", "Actions", "Status", "Key Facts"] {
            assert!(
                rendered.contains(&format!("### {section}")),
                "missing section: {section}"
            );
        }

        // Empty sections omitted
        let partial = CompactSummary {
            goals: vec!["only goal".into()],
            decisions: vec![],
            actions: vec![],
            status: vec![],
            key_facts: vec![],
        };
        let rendered = CompactResponse {
            summary: partial,
            facts: vec![],
        }
        .render_summary();
        assert!(rendered.contains("### Goals"));
        assert!(!rendered.contains("### Decisions"));
    }
}
