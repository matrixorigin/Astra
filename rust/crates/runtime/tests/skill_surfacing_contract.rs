//! Phase-3 contract tests for skill surfacing.
//!
//! Pre-Phase-3 plumbing was cache-hostile: `skill_tool_schema` embedded
//! the enum of skill names inside the tool schema, and
//! `skill_listing_system_message` rode the volatile system-reminder on
//! every turn. Adding or discovering a skill shifted schema bytes and
//! busted the Anthropic cache.
//!
//! Contracts this file pins:
//!   1. `skill_tool_schema_v2()` is a zero-arg function — the schema is
//!      a constant string of bytes. No skill list, no dynamic enum.
//!   2. The schema advertises `skill_name` as an open string (no `enum`)
//!      because the catalog is now surfaced through a separate listing
//!      section that lives in the cacheable prefix.
//!   3. `build_skill_listing_section(&[SkillToolInfo])` returns
//!      `Some(PromptSection { scope: CacheScope::Session, … })` and the
//!      text is byte-stable for equal inputs.
//!   4. `build_skill_listing_section` returns `None` for an empty skill
//!      list (don't emit a ghost `<available_skills>` block).
//!   5. Two sessions with the same catalog produce byte-identical
//!      listing text, even when passed in different order.
//!
//! The function / schema names here are the *target* API — if
//! `skill_tool_schema_v2` doesn't exist this file won't compile, which
//! is the whole point of TDD.

use astra_runtime::prompts::build_skill_listing_section;
use astra_runtime::turn::skill_tool::{SkillToolInfo, skill_tool_schema_v2};
use astra_turn_core::section_types::CacheScope;

fn skill(name: &str, desc: &str) -> SkillToolInfo {
    SkillToolInfo {
        name: name.into(),
        description: desc.into(),
        ..Default::default()
    }
}

// ── 1. Schema is zero-arg and constant ──────────────────────────────────────

#[test]
fn skill_tool_schema_v2_takes_no_skill_list() {
    // Bare compile check — the *signature* is the contract. If anyone adds
    // a parameter here, this test stops compiling and forces the reviewer
    // to justify the cache hit they just deleted.
    let _: serde_json::Value = skill_tool_schema_v2();
}

#[test]
fn skill_tool_schema_v2_is_byte_stable_across_calls() {
    let a = serde_json::to_vec(&skill_tool_schema_v2()).unwrap();
    let b = serde_json::to_vec(&skill_tool_schema_v2()).unwrap();
    assert_eq!(a, b, "schema must be byte-stable");
}

#[test]
fn skill_tool_schema_v2_has_open_string_skill_name_no_enum() {
    let schema = skill_tool_schema_v2();
    let params = &schema["function"]["parameters"];
    let skill_name = &params["properties"]["skill_name"];
    assert_eq!(
        skill_name["type"].as_str(),
        Some("string"),
        "skill_name must be a string"
    );
    assert!(
        skill_name.get("enum").is_none(),
        "skill_name must NOT carry an enum — that's what busts the cache: got {}",
        serde_json::to_string(skill_name).unwrap()
    );
}

#[test]
fn skill_tool_schema_v2_names_skill_tool_correctly() {
    let schema = skill_tool_schema_v2();
    assert_eq!(
        schema["function"]["name"].as_str(),
        Some("skill"),
        "schema name must stay 'skill'"
    );
}

// ── 2. Listing section lives in Session scope ───────────────────────────────

#[test]
fn skill_listing_section_is_session_scope() {
    let skills = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
    ];
    let section =
        build_skill_listing_section(&skills).expect("non-empty skills must produce a section");
    assert_eq!(
        section.scope,
        CacheScope::Session,
        "skill listing must be Session-scoped so it joins the cached prefix"
    );
}

#[test]
fn skill_listing_section_returns_none_when_empty() {
    assert!(
        build_skill_listing_section(&[]).is_none(),
        "empty skill list must not produce a ghost <available_skills> block"
    );
}

// ── 3. Byte stability ───────────────────────────────────────────────────────

#[test]
fn skill_listing_is_byte_stable_for_equal_inputs() {
    let skills = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
    ];
    let a = build_skill_listing_section(&skills).unwrap();
    let b = build_skill_listing_section(&skills).unwrap();
    assert_eq!(a.text, b.text, "listing must be byte-stable");
}

#[test]
fn skill_listing_is_stable_when_input_order_varies() {
    // Two sessions load the same catalog from providers that may emit in
    // different order. The listing must sort internally so both sessions
    // see the same bytes and hit the cache.
    let fwd = vec![
        skill("concise", "Output Constraint: Concise"),
        skill("markdown", "Output Format: Markdown"),
    ];
    let rev = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
    ];
    let a = build_skill_listing_section(&fwd).unwrap();
    let b = build_skill_listing_section(&rev).unwrap();
    assert_eq!(a.text, b.text, "input order must not affect listing bytes");
}

// ── 4. Content shape ────────────────────────────────────────────────────────

#[test]
fn skill_listing_wraps_in_available_skills_tags() {
    let section = build_skill_listing_section(&[skill("x", "desc of x")]).unwrap();
    assert!(section.text.contains("<available_skills>"));
    assert!(section.text.contains("</available_skills>"));
}

#[test]
fn skill_listing_emits_name_and_description_per_skill() {
    let skills = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
    ];
    let section = build_skill_listing_section(&skills).unwrap();
    for s in &skills {
        assert!(
            section.text.contains(&format!("<name>{}</name>", s.name)),
            "missing <name> for {}: got:\n{}",
            s.name,
            section.text
        );
        assert!(
            section.text.contains(&s.description),
            "missing description for {}: got:\n{}",
            s.name,
            section.text
        );
    }
}

#[test]
fn skill_listing_contains_skill_invocation_nudge() {
    let section = build_skill_listing_section(&[skill("markdown", "desc")]).unwrap();
    // Short nudge: "When a request matches, call the `skill` tool first."
    // Exact wording is flexible; what we assert is that the nudge is
    // present (not a naked list).
    assert!(
        section.text.contains("skill"),
        "listing must mention the skill invocation tool: got:\n{}",
        section.text
    );
}

/// REGRESSION (session 5e74f365): the listing nudge said
/// "call the `skill` tool with that skill's name FIRST (before any
/// other tool)". This was a hard imperative — it routed every request
/// matching a skill (e.g. `review-changes` matching "review latest
/// commit") through the skill, even when the user explicitly asked
/// for parallel agents ("多agents review", "3 agents review"). The
/// model never reached the agent spawn action.
///
/// Fix: soften the nudge so explicit user intent for parallel
/// fan-out wins over skill routing. The nudge now must mention BOTH
/// the parallel-agent override AND name the consolidated spawn syntax as the path
/// (so the model has a concrete next step, not just "don't use the
/// skill").
#[test]
fn skill_listing_nudge_carves_out_parallel_agent_intent() {
    let section =
        build_skill_listing_section(&[skill("review-changes", "Review code changes")]).unwrap();
    let body = &section.text;

    // The hard "FIRST (before any other tool)" imperative is gone.
    // Tolerate any case-insensitive variant of "FIRST" only when it's
    // qualified — assert the *unqualified* hard form is absent.
    assert!(
        !body.contains("FIRST (before any other tool)"),
        "the unqualified 'FIRST (before any other tool)' rule must be \
         removed — it overrode explicit user parallel-agent intent. \
         Got:\n{body}"
    );

    // The nudge MUST tell the model that parallel-agent requests
    // bypass skill routing and go to the consolidated agent spawn action.
    let lower = body.to_ascii_lowercase();
    assert!(
        lower.contains("parallel")
            || lower.contains("multi-agent")
            || lower.contains("multiple agents"),
        "nudge must name the parallel-agent override (the very intent \
         that was being silently routed through skills). Got:\n{body}"
    );
    assert!(
        body.contains("agent_fanout(action='start'")
            && body.contains("agent_fanout(action='get_results'"),
        "nudge must point at `agent_fanout(action='start', ...)` and \
          `agent_fanout(action='get_results', ...)` so the model has a concrete \
          atomic path when the user wants parallel fan-out. \
          Got:\n{body}"
    );
    assert!(
        !body.contains("agent.spawn")
            && !body.contains("agent.get_result")
            && !body.contains("agent(action='spawn', ...)"),
        "skill listing must actively reject the legacy dotted agent syntax. Got:\n{body}"
    );
}
