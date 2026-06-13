//! Permission system for tool approval and access control.
//!
//! This module provides the core permission infrastructure for Astra's
//! tool approval system, including rule evaluation, scope management,
//! audit logging, and cross-agent permission synchronization.

pub mod audit;
pub mod compound_command;
pub mod cwd_root;
pub mod engine;
pub mod match_target;
pub mod memory_profile;
pub mod notice;
pub mod path_glob;
pub mod path_sensitivity;
pub mod redact;
pub mod rule_grammar;
pub mod scope;
pub mod script_preview;
pub mod sync;
pub mod types;
