//! P0 contract: the CLI skill listing path must route the listing block
//! through its typed session owner, not copy it into generic stable or
//! volatile lanes. Duplicate ownership both wastes prompt budget and can
//! amplify irrelevant skill-routing metadata.
//!
//! The bridge reads `edge_profile["skill_listing_text"]` (populated by
//! the CLI host from `state.skills.listing_message`) and normalizes it for
//! `SessionContext.skill_listing_block`. The context binder then emits the
//! `AvailableSkills` section at `CacheScope::Session`.
//!
//! Full single-emission coverage lives at the bridge context assembly
//! boundary, where legacy generic copies are rejected.

use astra_runtime::turn::bridge::inprocess::skill_listing_block_for_edge_profile;

#[test]
fn non_empty_skill_listing_yields_one_normalized_typed_block() {
    let block = "<available_skills>\n  <skill><name>x</name></skill>\n</available_skills>";
    let normalized = skill_listing_block_for_edge_profile(Some(&format!(" \n{block}\n ")))
        .expect("non-empty listing must produce a typed block");
    assert_eq!(normalized, block);
}

#[test]
fn empty_skill_listing_yields_no_section() {
    assert!(skill_listing_block_for_edge_profile(None).is_none());
    assert!(skill_listing_block_for_edge_profile(Some("")).is_none());
}
