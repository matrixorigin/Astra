//! Text processing utilities extracted from the runtime crate.
//!
//! Provides tokenization and lexical semantic deduplication without runtime
//! infrastructure dependencies.

pub mod credential_redaction;
pub mod semantic_dedup;
pub mod str_preview;
pub mod text_tokenize;
pub mod tool_name;
pub mod url_component;
pub mod xml_escape;
