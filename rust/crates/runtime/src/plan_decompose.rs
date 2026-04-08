//! Backward-compatibility shim — re-exports everything from the new `plan` module.
//!
//! All plan types and functions have been consolidated into [`crate::plan`].
//! This module exists so that existing `plan_decompose::Foo` references
//! continue to compile without code changes.
#![allow(unused_imports)]

pub use crate::plan::*;
