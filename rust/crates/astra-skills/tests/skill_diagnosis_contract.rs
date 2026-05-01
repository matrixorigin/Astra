//! Contract: a `SkillDiagnosis` must be recoverable from the free-form
//! markdown response of an auto-invoked diagnostic skill
//! (`analyze_session`, `evaluate_session`, `optimize_prompt`).
//!
//! The skills themselves are LLM-authored and produce human-readable
//! markdown. The auto-invoke pipeline only needs the structured slice of
//! that output, so each SKILL.md instructs the LLM to append a
//! fenced JSON block (```skill-diagnosis ... ```) on auto-invoke.
//!
//! This contract locks down the parser's behaviour against:
//!   • well-formed responses (schema=2, all fields)
//!   • responses missing the block entirely (→ None, no panic)
//!   • responses where a stray preamble precedes the block
//!   • responses where multiple blocks appear (→ last valid one wins)
//!   • blocks with stale schema_version (→ None, force skill upgrade)
//!   • blocks with malformed JSON (→ None)
//!   • oversized fields (→ truncated to caps, parser still returns Some)

use astra_skills::auto_invoke::{SKILL_DIAGNOSIS_SCHEMA_VERSION, SkillDiagnosis};

// ─── Happy path ────────────────────────────────────────────────────────────

#[test]
fn parses_well_formed_skill_diagnosis_block() {
    let output = r#"
# Session Analysis

The agent stalled on `grep` repeatedly in the `src/` subtree.

Recommend narrowing scope or switching to `rg`.

```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "agent looping on grep in deep subtree",
  "findings": [
    "grep invoked 4× with identical args",
    "no new matches since turn 3"
  ],
  "recommended_action": "switch to rg or narrow scope to src/",
  "success_criteria": [
    {
      "metric": "session_stalls_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "session stalls stop increasing"
    }
  ],
  "source": "real_skill"
}
```
"#;

    let diag = SkillDiagnosis::parse_from_skill_output(output).expect("should parse");
    assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
    assert_eq!(diag.skill, "analyze_session");
    assert_eq!(diag.cause, "session_stalls");
    assert_eq!(diag.success_criteria.len(), 1);
    assert_eq!(diag.headline, "agent looping on grep in deep subtree");
    assert_eq!(diag.findings.len(), 2);
    assert_eq!(diag.findings[0], "grep invoked 4× with identical args");
    assert_eq!(
        diag.recommended_action.as_deref(),
        Some("switch to rg or narrow scope to src/")
    );
}

#[test]
fn parses_block_with_no_recommended_action() {
    let output = r#"
```skill-diagnosis
{
  "schema_version": 2,
  "skill": "evaluate_session",
  "cause": "repeated_corrections",
  "headline": "user re-scoped 5× in 8 turns",
  "findings": ["scope drift"],
  "success_criteria": [
    {
      "metric": "corrections_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "new corrections stop increasing"
    }
  ],
  "source": "real_skill"
}
```
"#;
    let diag = SkillDiagnosis::parse_from_skill_output(output).expect("parse");
    assert!(diag.recommended_action.is_none());
    assert_eq!(diag.skill, "evaluate_session");
}

// ─── Missing / unparseable inputs ─────────────────────────────────────────

#[test]
fn returns_none_when_block_is_absent() {
    let output = "Here is some analysis prose with no structured block.";
    assert!(SkillDiagnosis::parse_from_skill_output(output).is_none());
}

#[test]
fn returns_none_on_empty_input() {
    assert!(SkillDiagnosis::parse_from_skill_output("").is_none());
}

#[test]
fn returns_none_when_block_has_malformed_json() {
    let output = r#"
```skill-diagnosis
{ this is not json }
```
"#;
    assert!(SkillDiagnosis::parse_from_skill_output(output).is_none());
}

#[test]
fn returns_none_when_schema_version_is_unsupported() {
    // Future-dated schema version — parser must refuse and force a skill upgrade
    // rather than silently accept a payload it cannot interpret.
    let output = r#"
```skill-diagnosis
{
  "schema_version": 99,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "stub",
  "findings": []
}
```
"#;
    assert!(SkillDiagnosis::parse_from_skill_output(output).is_none());
}

#[test]
fn returns_none_when_required_fields_missing() {
    let output = r#"
```skill-diagnosis
{
  "schema_version": 2,
  "headline": "incomplete"
}
```
"#;
    assert!(SkillDiagnosis::parse_from_skill_output(output).is_none());
}

// ─── Ambiguity / robustness ────────────────────────────────────────────────

#[test]
fn multiple_blocks_use_the_last_one() {
    // If the LLM hedges with two candidate diagnoses, the final block is
    // the authoritative one — mirroring how "the agent's last word" is the
    // commit in other parts of the runtime (e.g. last reflection wins).
    let output = r#"
```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "first guess",
  "findings": []
}
```

More analysis follows.

```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "final guess",
  "findings": ["definitive finding"],
  "success_criteria": [
    {
      "metric": "session_stalls_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "session stalls stop increasing"
    }
  ],
  "source": "real_skill"
}
```
"#;
    let diag = SkillDiagnosis::parse_from_skill_output(output).expect("parse");
    assert_eq!(diag.headline, "final guess");
    assert_eq!(diag.findings, vec!["definitive finding".to_string()]);
}

#[test]
fn oversized_fields_are_truncated_by_parser() {
    // The skill might produce a 500-char headline / 20 findings. The parser
    // must still return Some, with the payload already shrunk to caps.
    let long_finding = "x".repeat(500);
    let findings_json = (0..20)
        .map(|i| format!(r#""finding {i}: {long}""#, long = long_finding))
        .collect::<Vec<_>>()
        .join(",");
    let output = format!(
        r#"
```skill-diagnosis
{{
   "schema_version": 2,
   "skill": "optimize_prompt",
   "cause": "budget_pressure",
   "headline": "{}",
   "findings": [{findings_json}],
   "success_criteria": [
     {{
       "metric": "budget_pressure",
       "operator": "lte",
       "threshold": 0.85,
       "window_turns": 3,
       "description": "{}"
     }}
   ],
   "source": "real_skill"
 }}
```
"#,
        "y".repeat(500),
        "z".repeat(500),
    );
    let diag = SkillDiagnosis::parse_from_skill_output(&output).expect("parse");
    assert!(diag.headline.chars().count() <= astra_skills::auto_invoke::MAX_HEADLINE_LEN);
    assert!(diag.findings.len() <= astra_skills::auto_invoke::MAX_FINDINGS);
    for f in &diag.findings {
        assert!(f.chars().count() <= astra_skills::auto_invoke::MAX_FINDING_LEN);
    }
}

#[test]
fn returns_none_when_cause_tag_is_unknown() {
    // Auto-invoke defines only three causes. A payload claiming a fourth
    // cause points to a skill bug or a protocol drift — fail closed rather
    // than silently accept it.
    let output = r#"
```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "cosmic_rays",
  "headline": "hmm",
  "findings": []
}
```
"#;
    assert!(SkillDiagnosis::parse_from_skill_output(output).is_none());
}

#[test]
fn skill_md_example_blocks_are_parseable() {
    // Dog-food: every SKILL.md ships an example JSON block for LLM authors
    // to copy. If a future edit breaks the example, this test fails early.
    // CARGO_MANIFEST_DIR points at rust/crates/astra-skills; walk up to the
    // repo root where the top-level `skills/` directory lives.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let repo_root = std::path::Path::new(manifest_dir)
        .ancestors()
        .nth(3)
        .expect("walk up to repo root");

    for skill_name in ["analyze_session", "evaluate_session", "optimize_prompt"] {
        let path = repo_root.join("skills").join(skill_name).join("SKILL.md");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let diag = SkillDiagnosis::parse_from_skill_output(&text)
            .unwrap_or_else(|| panic!("{}: example block must parse", path.display()));
        assert_eq!(
            diag.skill,
            skill_name,
            "{}: example block's `skill` field must match dir name",
            path.display()
        );
        assert_eq!(diag.schema_version, SKILL_DIAGNOSIS_SCHEMA_VERSION);
        assert!(!diag.headline.is_empty());
    }
}

#[test]
fn parser_ignores_prose_between_fences() {
    // Real skill output will have paragraphs before the JSON. Parser must
    // locate the block regardless of leading content.
    let output = r#"
# Long Analysis Preamble

Lorem ipsum dolor sit amet, consectetur adipiscing elit.
Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.

Some findings:
- Something
- Something else

```skill-diagnosis
{
  "schema_version": 2,
  "skill": "analyze_session",
  "cause": "session_stalls",
  "headline": "found it",
  "findings": [],
  "success_criteria": [
    {
      "metric": "session_stalls_delta",
      "operator": "lte",
      "threshold": 0.0,
      "window_turns": 3,
      "description": "session stalls stop increasing"
    }
  ],
  "source": "real_skill"
}
```

Closing remarks follow.
"#;
    let diag = SkillDiagnosis::parse_from_skill_output(output).expect("parse");
    assert_eq!(diag.headline, "found it");
}
