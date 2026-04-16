//! Text processing utilities extracted from the runtime crate.
//!
//! Provides tokenization, semantic deduplication (TF-IDF based), and
//! output style loading — all with zero runtime infrastructure deps.

pub mod output_style;
pub mod semantic_dedup;
pub mod text_tokenize;
