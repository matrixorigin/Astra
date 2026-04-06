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
