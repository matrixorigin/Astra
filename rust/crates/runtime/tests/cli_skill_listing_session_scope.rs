//! P0 contract: the CLI skill listing path must route the listing block
//! into the **session-stable** lane, not the volatile lane. Without this
//! fix, CLI users get zero cache benefit from the skill surface rewrite.
//!
//! The bridge reads `edge_profile["skill_listing_text"]` (populated by
//! the CLI host from `state.skills.listing_message`) and composes it into
//! one of two lanes:
//!   - `stable_sections` → ExternalSources.extra_stable_sections → bound
//!     into RuntimeIdentity (Session scope) in the cached prefix.
//!   - `dynamic_sections` → ExternalSources.extra_dynamic_sections →
//!     bound into RuntimeVolatile (None scope) in the volatile tail.
//!
//! Red test: run the same section selection logic the bridge uses and
//! assert the skill listing lands in the stable lane.

use astra_runtime::prompts::{CacheScope, PromptSection};

/// Mirror of the bridge's skill-listing lane selection. Extracted as a
/// pure helper so the bridge can delegate to it and tests have something
/// to call. Production call site: `bridge_inprocess.rs:~1590`.
///
/// This function does not yet exist — that's the red.
use astra_runtime::turn::bridge_inprocess::skill_listing_section_for_edge_profile;

#[test]
fn non_empty_skill_listing_yields_session_scope_section() {
    let block = "<available_skills>\n  <skill><name>x</name></skill>\n</available_skills>";
    let section: Option<PromptSection> = skill_listing_section_for_edge_profile(Some(block));
    let s = section.expect("non-empty listing must produce a section");
    assert_eq!(
        s.scope,
        CacheScope::Session,
        "skill listing must be Session-scope so it joins the cached prefix; got {:?}",
        s.scope
    );
    assert!(
        s.text.contains("<available_skills>"),
        "section text must contain the block contents"
    );
}

#[test]
fn empty_skill_listing_yields_no_section() {
    assert!(skill_listing_section_for_edge_profile(None).is_none());
    assert!(skill_listing_section_for_edge_profile(Some("")).is_none());
}
