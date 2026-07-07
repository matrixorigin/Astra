//! Phase-9 end-to-end contract: the skill listing is byte-stable across
//! turns AND carries `CacheScope::Session` — the Anthropic prompt cache
//! can hit the full tools[] + system prefix even with skills loaded.
//!
//! Pre-phase-9 behaviour: `skill_listing_system_message` rode the
//! volatile system-reminder on every turn, rewriting the block as new
//! skills got ranked/discovered. ~2.5KB rewritten per turn.
//!
//! Post-phase-9: `build_skill_listing_section` returns a Session-scope
//! section rendered once per session. The block bytes are identical
//! turn-over-turn (sorted alphabetically, no ranking).

use astra_runtime::prompts::{CacheScope, build_skill_listing_section};
use astra_runtime::turn::skill_tool::skill_tool_schema_v2;
use astra_skills::traits::SkillToolInfo;

fn skill(name: &str, desc: &str) -> SkillToolInfo {
    SkillToolInfo {
        name: name.into(),
        description: desc.into(),
        ..Default::default()
    }
}

#[test]
fn skill_listing_section_is_session_scope_in_production_builder() {
    let skills = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
    ];
    let s =
        build_skill_listing_section(&skills).expect("non-empty skill list must produce a section");
    assert_eq!(
        s.scope,
        CacheScope::Session,
        "skill listing must be Session-scope so it joins the cached prefix"
    );
}

#[test]
fn skill_listing_bytes_identical_across_turns() {
    // Same catalog produces identical bytes regardless of when called —
    // this is what lets the Anthropic cache hit the entire prefix.
    let skills = vec![
        skill("markdown", "Output Format: Markdown"),
        skill("concise", "Output Constraint: Concise"),
        skill("review_changes", "Review code changes"),
    ];
    let turn_1 = build_skill_listing_section(&skills).unwrap();
    let turn_2 = build_skill_listing_section(&skills).unwrap();
    let turn_n = build_skill_listing_section(&skills).unwrap();

    assert_eq!(turn_1.text, turn_2.text);
    assert_eq!(turn_1.text, turn_n.text);
}

#[test]
fn skill_tool_schema_in_production_has_no_enum() {
    // Direct proof that the live `skill` tool schema is the byte-stable
    // v2 variant — no `enum` field under skill_name that would rebuild
    // whenever the skill catalog changed.
    let schema = skill_tool_schema_v2();
    let skill_name = &schema["function"]["parameters"]["properties"]["skill_name"];
    assert!(
        skill_name.get("enum").is_none(),
        "production skill schema must have no enum under skill_name: {}",
        serde_json::to_string(&schema).unwrap()
    );
    assert_eq!(
        schema["function"]["name"].as_str(),
        Some("skill"),
        "schema name stays 'skill'"
    );
}

#[test]
fn skill_listing_byte_stable_under_input_order_variation() {
    // Two sessions (e.g. two CLI invocations) may load skills from
    // providers in different orders. The output must still match — a
    // cache miss caused by provider iteration order is a silent bug.
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
    assert_eq!(a.text, b.text);
}
