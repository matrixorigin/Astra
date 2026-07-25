//! Text processing utilities extracted from the runtime crate.
//!
//! Provides tokenization, lexical semantic deduplication, and output style
//! loading — all with zero runtime infrastructure deps.

pub mod output_style;
pub mod semantic_dedup;
pub mod str_preview;
pub mod text_tokenize;
pub mod tool_name;
pub mod url_component;
pub mod xml_escape;
