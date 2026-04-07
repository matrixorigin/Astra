/// Prompt for extracting structured facts from a conversation summary.
///
/// Each fact is one self-contained sentence suitable for long-term memory.
/// The LLM outputs a JSON array so we can store each fact as a separate
/// Memoria entry with the appropriate `memory_type`.
pub const MEMORY_EXTRACTOR_PROMPT: &str = "\
Extract key facts worth remembering from the following conversation summary.

## Output format
Return a JSON array. Each element has two fields:
- \"fact\": a single self-contained sentence (≤30 words).
- \"type\": one of \"semantic\" (general knowledge/preference), \"profile\" (user info), \
\"procedural\" (how-to / convention), \"working\" (transient project state).

## Rules
- 3-8 facts maximum. Fewer is better if the summary is short.
- Each fact must stand alone without the surrounding conversation.
- Prefer user preferences, project conventions, decisions, and recurring patterns.
- DO NOT extract: transient errors, file contents, raw tool output, one-off commands.
- If no facts are worth remembering, return an empty array: []

## Example
[
  {\"fact\": \"User prefers Rust for CLI tools.\", \"type\": \"profile\"},
  {\"fact\": \"Project uses cargo workspace at rust/crates/.\", \"type\": \"procedural\"}
]

Summary to extract from:
";

/// Parse the LLM response from the memory extractor into fact+type pairs.
///
/// Returns `Vec<(fact_text, memory_type)>`. Tolerates markdown fences and
/// trailing commas since LLMs are sloppy JSON producers.
pub fn parse_extracted_facts(raw: &str) -> Vec<(String, String)> {
    // Strip markdown code fences if present
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

    // Try to parse as JSON array
    let arr: Vec<serde_json::Value> = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => {
            // Try extracting just the array portion (LLM may add preamble)
            if let Some(start) = json_str.find('[') {
                if let Some(end) = json_str.rfind(']') {
                    let slice = &json_str[start..=end];
                    serde_json::from_str(slice).unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
    };

    arr.iter()
        .filter_map(|item| {
            let fact = item
                .get("fact")
                .and_then(|v| v.as_str())?
                .trim()
                .to_string();
            let mem_type = item
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("semantic")
                .to_string();
            if fact.is_empty() {
                return None;
            }
            // Validate memory_type
            let valid_type = match mem_type.as_str() {
                "semantic" | "profile" | "procedural" | "working" => mem_type,
                _ => "semantic".to_string(),
            };
            Some((fact, valid_type))
        })
        .collect()
}

/// User message sent to the LLM to produce a compact session summary.
///
/// The existing conversation history is already in the message context, so the
/// LLM sees the full transcript when it generates the summary.
pub const COMPACT_SUMMARY_REQUEST: &str = "\
Produce a concise, structured summary of the conversation so far.

## Required sections (use these exact headers):

### Goals
What the user is trying to achieve.

### Decisions
Key choices made and their reasoning. One bullet per decision.

### Actions
Tool calls, code changes, commands run. Brief, factual.

### Status
Current state: what's done, what's pending, any blockers.

### Key Facts
User preferences, project conventions, or important details worth remembering.

## Rules
- Under 250 words total.
- Output only the five sections above. Do not add any other headings, sections, preamble, greeting, or sign-off.
- The first line of your response must be exactly `### Goals` (no text before it).
- After the bullets under `### Key Facts`, stop. Do not add closing remarks or a summary line.
- Use bullet points under each section, not prose paragraphs.
- Do not paste the raw transcript; summarize.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_array() {
        assert!(parse_extracted_facts("[]").is_empty());
    }

    #[test]
    fn parse_valid_json() {
        let input = r#"[{"fact": "Uses Rust.", "type": "procedural"}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].0, "Uses Rust.");
        assert_eq!(facts[0].1, "procedural");
    }

    #[test]
    fn parse_with_markdown_fences() {
        let input = "```json\n[{\"fact\": \"Prefers vim.\", \"type\": \"profile\"}]\n```";
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1, "profile");
    }

    #[test]
    fn parse_with_plain_fences() {
        let input = "```\n[{\"fact\": \"Test.\", \"type\": \"semantic\"}]\n```";
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
    }

    #[test]
    fn parse_with_preamble() {
        let input = "Here are the facts:\n[{\"fact\": \"Preamble test.\", \"type\": \"working\"}]";
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].1, "working");
    }

    #[test]
    fn parse_invalid_type_defaults_semantic() {
        let input = r#"[{"fact": "Test.", "type": "invalid_type"}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_missing_type_defaults_semantic() {
        let input = r#"[{"fact": "No type field."}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_empty_fact_filtered() {
        let input = r#"[{"fact": "", "type": "semantic"}, {"fact": "  ", "type": "semantic"}]"#;
        let facts = parse_extracted_facts(input);
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_missing_fact_field_filtered() {
        let input = r#"[{"type": "semantic"}]"#;
        let facts = parse_extracted_facts(input);
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_garbage_input() {
        assert!(parse_extracted_facts("not json at all").is_empty());
    }

    #[test]
    fn parse_no_brackets() {
        assert!(parse_extracted_facts("just some text without brackets").is_empty());
    }

    #[test]
    fn parse_all_valid_types() {
        for t in &["semantic", "profile", "procedural", "working"] {
            let input = format!(r#"[{{"fact": "Test.", "type": "{t}"}}]"#);
            let facts = parse_extracted_facts(&input);
            assert_eq!(facts[0].1, *t);
        }
    }

    #[test]
    fn parse_multiple_facts() {
        let input = r#"[
            {"fact": "One.", "type": "semantic"},
            {"fact": "Two.", "type": "profile"},
            {"fact": "Three.", "type": "procedural"}
        ]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 3);
    }

    #[test]
    fn parse_fact_trimmed() {
        let input = r#"[{"fact": "  spaced  ", "type": "semantic"}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts[0].0, "spaced");
    }

    // --- parse_extracted_facts edge cases ---

    #[test]
    fn parse_fact_with_newlines_in_text() {
        let input = r#"[{"fact": "line1\nline2", "type": "semantic"}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].0.contains('\n')); // JSON \n decoded to actual newline
    }

    #[test]
    fn parse_fact_with_unicode() {
        let input = r#"[{"fact": "使用JWT进行身份验证", "type": "semantic"}]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].0.contains("JWT"));
    }

    #[test]
    fn parse_duplicate_facts_both_kept() {
        let input = r#"[
            {"fact": "same fact", "type": "semantic"},
            {"fact": "same fact", "type": "semantic"}
        ]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 2); // no dedup in parser
    }

    #[test]
    fn parse_fact_type_whitespace_defaults() {
        let input = r#"[{"fact": "test", "type": " semantic "}]"#;
        let facts = parse_extracted_facts(input);
        // " semantic " doesn't match any valid type → defaults to "semantic"
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_deeply_nested_array_rejected() {
        let input = "[[[[]]]]";
        let facts = parse_extracted_facts(input);
        assert!(facts.is_empty());
    }

    #[test]
    fn parse_fact_with_all_valid_types() {
        let input = r#"[
            {"fact": "a", "type": "semantic"},
            {"fact": "b", "type": "profile"},
            {"fact": "c", "type": "procedural"},
            {"fact": "d", "type": "working"}
        ]"#;
        let facts = parse_extracted_facts(input);
        assert_eq!(facts.len(), 4);
        assert_eq!(facts[0].1, "semantic");
        assert_eq!(facts[1].1, "profile");
        assert_eq!(facts[2].1, "procedural");
        assert_eq!(facts[3].1, "working");
    }

    #[test]
    fn parse_fact_number_type_defaults_semantic() {
        let input = r#"[{"fact": "test", "type": 123}]"#;
        let facts = parse_extracted_facts(input);
        // type is a number, not a string → as_str() returns None → defaults to "semantic"
        assert_eq!(facts[0].1, "semantic");
    }

    #[test]
    fn parse_with_trailing_comma_fails_gracefully() {
        let input = r#"[{"fact": "test", "type": "semantic"},]"#;
        let facts = parse_extracted_facts(input);
        // serde_json rejects trailing commas → empty
        assert!(facts.is_empty());
    }
}
