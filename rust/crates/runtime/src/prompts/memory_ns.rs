//! Well-known memory namespaces mapped to Memoria memory_types.
//!
//! Namespaces provide semantic structure on top of flat memory storage.
//! Content is prefixed with `[namespace:]` for easy filtering.

/// User preferences and habits ("profile" memory_type)
pub const PREFERENCE: &str = "preference";
/// Learned knowledge and patterns ("semantic" memory_type)
pub const KNOWLEDGE: &str = "knowledge";
/// In-progress work staging area ("working" memory_type)
pub const STAGING: &str = "staging";
/// Persistent plans across sessions ("procedural" memory_type)
pub const PLAN: &str = "plan";
/// Task entries with status ("working" memory_type)
pub const TASK: &str = "task";
/// Session summaries ("episodic" memory_type)
pub const EPISODIC: &str = "episodic";

/// Map namespace to Memoria memory_type.
pub fn to_memory_type(ns: &str) -> &'static str {
    match ns {
        PREFERENCE => "profile",
        KNOWLEDGE => "semantic",
        STAGING => "working",
        PLAN => "procedural",
        TASK => "working",
        EPISODIC => "episodic",
        _ => "semantic",
    }
}

/// All known namespaces.
pub const ALL: &[&str] = &[PREFERENCE, KNOWLEDGE, STAGING, PLAN, TASK, EPISODIC];
