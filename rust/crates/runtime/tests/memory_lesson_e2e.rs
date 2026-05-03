//! Consolidated E2E tests for the memory + lesson subsystem.
//!
//! Each submodule was previously a separate integration test file. Merging
//! them into one binary reduces link passes from 5 to 1, preventing linker
//! OOM on CI runners with limited RAM.

mod memory_lesson_e2e {
    #[path = "memory_types_e2e.rs"]
    mod memory_types;

    #[path = "lesson_l3_integration.rs"]
    mod lesson_l3;

    #[path = "memory_prompt_assembly_e2e.rs"]
    mod memory_prompt;

    #[path = "self_model_lessons.rs"]
    mod self_model_lessons;

    #[path = "self_model_skill_diagnosis.rs"]
    mod self_model_diagnosis;
}
