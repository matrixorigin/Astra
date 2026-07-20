mod inference;
pub mod relevance;

#[cfg(test)]
pub(crate) use inference::DirectMemoryInferenceClient;
pub(crate) use inference::DurableMemoryInferenceClient;
pub use inference::{MemoryInferenceClient, MemoryInferencePort, MemoryInferenceRequest};
pub use relevance::{
    MEMORY_FEEDBACK_FILTER_PROMPT, RELEVANCE_FILTER_PROMPT, build_memory_feedback_query,
    build_relevance_query, select_dismissed_memory_indices,
};
