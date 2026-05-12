//! Post-turn hallucination tripwire.
//!
//! Detects when an assistant-output summary describes a tool result
//! that never actually happened — specifically the "silently
//! returned {}", "silently skipped", "silently dropped" family of
//! confabulated outcomes. The cascade that motivated this module
//! (session 6d6c1041 turn 8 → 5933ebce) went:
//!
//! 1. User casually mis-described a tool failure as `returned {}`.
//! 2. The LLM picked up the phrase and repeated it in its own
//!    summary, grafting a theory onto it ("tool runner bug swallows
//!    output", "3889 passed is unreliable", etc.)
//! 3. The narrative propagated into the next session as "astra
//!    has a `{}` problem" even though no tool call had ever
//!    returned `{}` on the wire.
//!
//! The fix is defensive: when the assistant's own prose claims
//! a phantom outcome, check the real tool-call records. If none
//! match, produce a nudge that the next turn's system context can
//! inject so the LLM sees the contradiction and self-corrects.
//!
//! Pure function: no I/O, no async, one `detect` entry-point.

/// A single tool-call observation the tripwire needs: just the
/// result body the LLM would have seen. Keeping the input minimal
/// decouples this module from the full `ToolCallRecord` shape.
#[derive(Debug, Clone)]
pub struct TripwireToolObservation<'a> {
    pub name: &'a str,
    pub result_preview: &'a str,
}

/// The phrases we classify as "hallucinated outcome language".
/// Short, literal substrings — we match on presence, not regex, to
/// keep the matcher obvious and avoid false positives from quoted
/// discussion of the phrases themselves (which is rare in
/// assistant output).
const HALLUCINATED_PHRASES: &[&str] = &[
    "silently returned {}",
    "silently returned \"{}\"",
    "silently skipped",
    "silently dropped",
    "silently swallowed",
    "returned empty {}",
    "returned an empty {}",
];

/// Outcome of the tripwire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TripwireVerdict {
    /// Assistant output matches what the tool calls actually
    /// produced — nothing to do.
    Clean,
    /// Assistant output used hallucinated-outcome phrasing but no
    /// tool call this turn actually produced a `{}` / empty-body
    /// result. Contains a nudge string suitable for injection into
    /// the next turn's system context.
    Mismatch {
        nudge: String,
        matched_phrases: Vec<String>,
    },
}

/// Detect a hallucination mismatch.
///
/// - `assistant_output`: the model's final prose for this turn.
/// - `tool_calls`: the turn's tool-call observations, in order.
///
/// Returns `Clean` when either (a) no hallucinated phrasing is
/// present in the prose, or (b) a tool call in this turn actually
/// produced a `{}`-shaped / empty-body result that could
/// legitimately anchor the prose.
///
/// The tripwire is conservative on purpose: if any tool call body
/// plausibly matches `{}` / empty output, we stay silent. We only
/// fire when the prose *invents* an outcome that has no physical
/// correlate in this turn's tool results.
pub fn detect<'a>(
    assistant_output: &str,
    tool_calls: impl IntoIterator<Item = TripwireToolObservation<'a>>,
) -> TripwireVerdict {
    // Strip fenced code blocks and blockquoted lines before matching.
    // Without this, an assistant that legitimately *quotes* a prior
    // turn's tripwire nudge ("last turn the system said 'silently
    // returned {}'…") would re-fire the wire and create a self-
    // quoting loop. We only want to catch the model using these
    // phrases in its *own* narrative voice.
    let prose = strip_quoted_regions(assistant_output);
    let lower = prose.to_ascii_lowercase();
    let matched: Vec<String> = HALLUCINATED_PHRASES
        .iter()
        .filter(|p| lower.contains(&p.to_ascii_lowercase()))
        .map(|p| (*p).to_string())
        .collect();
    if matched.is_empty() {
        return TripwireVerdict::Clean;
    }

    // Anchor check: is there any tool call whose result_preview
    // really *is* `{}` or an empty body? If so, the prose is
    // describing something real — let it stand.
    let any_physical_empty = tool_calls.into_iter().any(|c| {
        let trimmed = c.result_preview.trim();
        trimmed == "{}" || trimmed.is_empty()
    });
    if any_physical_empty {
        return TripwireVerdict::Clean;
    }

    TripwireVerdict::Mismatch {
        nudge: build_nudge(&matched),
        matched_phrases: matched,
    }
}

/// Remove fenced code blocks (```...```) and blockquote lines
/// (leading `>` after optional whitespace) from `input`. Used to
/// avoid the tripwire echoing itself when the assistant quotes a
/// previous nudge. Keeps newlines so line-based phrase matches
/// still work over the surviving prose.
fn strip_quoted_regions(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        out.push_str(line);
    }
    out
}

fn build_nudge(matched: &[String]) -> String {
    let list = matched
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "⚠ Self-check: your previous turn claimed {list} but no tool call \
         this turn actually returned `{{}}` or an empty body. Quote the \
         tool's real error/output verbatim in your next summary — don't \
         coin phantom outcome labels.  If in doubt, run `introspect \
         subtopic=recent` or re-read the tool-call history before \
         narrating."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs<'a>(name: &'a str, preview: &'a str) -> TripwireToolObservation<'a> {
        TripwireToolObservation {
            name,
            result_preview: preview,
        }
    }

    #[test]
    fn clean_when_no_hallucinated_phrases() {
        let v = detect(
            "I ran the build and it passed cleanly.",
            [obs("bash", "cargo build: Finished")],
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn fires_on_returned_empty_braces_phrase_with_no_anchor() {
        // Regression (session 5933ebce): LLM wrote "str_replace on
        // policy.rs silently returned {}" when the tool had
        // actually returned "Error: edit[0] old_str not found."
        let v = detect(
            "The str_replace edits to policy.rs silently returned {} — \
             the three sites still have .clone().",
            [obs(
                "str_replace",
                "Error: edit[0] old_str not found. Aborting all edits.",
            )],
        );
        match v {
            TripwireVerdict::Mismatch {
                nudge,
                matched_phrases,
            } => {
                assert!(
                    matched_phrases
                        .iter()
                        .any(|p| p.contains("silently returned {}")),
                    "expected phrase match, got {matched_phrases:?}"
                );
                assert!(nudge.contains("Self-check"));
                assert!(nudge.contains("tool's real error"));
            }
            TripwireVerdict::Clean => panic!("tripwire should have fired"),
        }
    }

    #[test]
    fn fires_on_silently_skipped_with_real_error_on_the_wire() {
        let v = detect(
            "The edits silently skipped.",
            [obs("str_replace", "Error: old_str not found")],
        );
        assert!(matches!(v, TripwireVerdict::Mismatch { .. }));
    }

    #[test]
    fn stays_clean_when_tool_actually_returned_empty_braces() {
        // If a tool's result_preview really is `{}`, the prose is
        // describing reality and we must NOT nag the LLM.
        let v = detect(
            "That subtool silently returned {}.",
            [obs("some_tool", "{}")],
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn stays_clean_when_tool_returned_empty_body() {
        // Same story for an empty string preview — the LLM's
        // "returned empty" claim is accurate.
        let v = detect(
            "The helper returned an empty {} object.",
            [obs("some_tool", "")],
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn case_insensitive_phrase_match() {
        // The detector folds case so capitalized variants still
        // trip the wire.
        let v = detect(
            "SILENTLY RETURNED {} — something went wrong.",
            [obs("str_replace", "Error: old_str not found")],
        );
        assert!(matches!(v, TripwireVerdict::Mismatch { .. }));
    }

    #[test]
    fn all_matched_phrases_surface_in_verdict() {
        // If multiple phrases appear, all are reported so the
        // nudge can enumerate them.
        let v = detect(
            "The tool silently returned {} and silently skipped the edit.",
            [obs("str_replace", "Error: something else")],
        );
        match v {
            TripwireVerdict::Mismatch {
                matched_phrases, ..
            } => {
                assert!(matched_phrases.len() >= 2, "got {matched_phrases:?}");
            }
            _ => panic!("should have fired"),
        }
    }

    #[test]
    fn ignores_phrase_inside_fenced_code_block() {
        // An assistant legitimately quoting a previous nudge inside
        // a ```-fence must not retrigger the wire.
        let v = detect(
            "Earlier the system warned me:\n\n```\nYour prose said 'silently returned {}'.\n```\n\nI'll avoid that phrasing going forward.",
            [obs("str_replace", "Error: old_str not found")],
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn unclosed_fence_swallows_rest_of_output() {
        // Contract pin: an unclosed ```fence strips everything that
        // follows — safer to under-match than over-match (false
        // negative preferable to false positive for the tripwire).
        // Without this, a truncated assistant output containing a
        // quoted prior nudge + live confabulation would re-fire.
        let out = "Context:\n```rust\nsilently returned {} in prior turn\n// fence never closes";
        let v = detect(out, std::iter::empty());
        assert!(
            matches!(v, TripwireVerdict::Clean),
            "unclosed fence must strip trailing prose (got {v:?})"
        );
    }

    #[test]
    fn ignores_phrase_inside_blockquote() {
        let v = detect(
            "> last turn: silently returned {}\n\nI won't use that phrasing.",
            [obs("str_replace", "Error: old_str not found")],
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn clean_on_empty_assistant_output() {
        let v = detect("", [obs("bash", "ok")]);
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn clean_with_no_tool_calls_when_output_has_no_hallucination() {
        // Zero tool calls + clean prose → Clean.
        let v: TripwireVerdict = detect(
            "I'll wait for your confirmation before continuing.",
            std::iter::empty(),
        );
        assert_eq!(v, TripwireVerdict::Clean);
    }

    #[test]
    fn fires_when_no_tool_calls_but_prose_claims_a_silent_return() {
        // If prose claims a silent empty return but there are NO
        // tool calls to reference, the claim is definitely
        // ungrounded.
        let v: TripwireVerdict = detect(
            "The last edit silently returned {} again.",
            std::iter::empty(),
        );
        assert!(matches!(v, TripwireVerdict::Mismatch { .. }));
    }
}
