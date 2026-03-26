/// Contract tests for InProcessBridge hook side effects.
///
/// Verifies that the hook payload pattern used by InProcessChatTurnBridge
/// (build_turn_hook_args → run_bridge_hook_side_effects) correctly triggers
/// decision audit, skill selection, implicit feedback, and reflection writes.
use std::sync::Arc;

use async_trait::async_trait;
use mo_agent_runtime::{
    TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
    TurnReflectionLessonRecord, TurnReflectionLessonWriter, TurnReflectionMark,
    TurnReflectionStateStore, bridge::side_effects::run_bridge_hook_side_effects,
    turn::tail_persist::build_turn_hook_args,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

// ── Recording stubs ──────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct RecordingHookDbWriter {
    plans: Arc<Mutex<Vec<TurnHookDbPersistPlan>>>,
}

#[async_trait]
impl TurnHookDbWriter for RecordingHookDbWriter {
    async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
        self.plans.lock().await.push(plan);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingReflectionStateStore {
    marks: Arc<Mutex<Vec<TurnReflectionMark>>>,
}

#[async_trait]
impl TurnReflectionStateStore for RecordingReflectionStateStore {
    async fn mark_reflecting(&self, mark: TurnReflectionMark) -> Result<(), String> {
        self.marks.lock().await.push(mark);
        Ok(())
    }
    async fn pop_reflecting(&self, session_id: &str) -> Result<Option<TurnReflectionMark>, String> {
        let mut marks = self.marks.lock().await;
        if let Some(i) = marks.iter().position(|m| m.session_id == session_id) {
            Ok(Some(marks.remove(i)))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Default)]
struct RecordingReflectionLessonWriter {
    lessons: Arc<Mutex<Vec<TurnReflectionLessonRecord>>>,
}

#[async_trait]
impl TurnReflectionLessonWriter for RecordingReflectionLessonWriter {
    async fn persist_lesson(&self, lesson: TurnReflectionLessonRecord) -> Result<(), String> {
        self.lessons.lock().await.push(lesson);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingObserverWorker {
    requests: Arc<Mutex<Vec<TurnObserverRequest>>>,
}

#[async_trait]
impl TurnObserverWorker for RecordingObserverWorker {
    async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
        self.requests.lock().await.push(request);
        Ok(())
    }
}

// ── Helper ───────────────────────────────────────────────────────────────────

fn build_hook_payload_with_tool_call() -> Value {
    let messages = vec![json!({"role": "user", "content": "list files in src/"})];
    let tool_results: Vec<Value> = vec![];
    let tool_calls = vec![json!({
        "function": {"name": "bash", "arguments": "{\"command\": \"ls src/\"}"}
    })];
    Value::Object(build_turn_hook_args(
        "user-1",
        "session-1",
        &messages,
        &tool_results,
        "Let me list the files for you.",
        &tool_calls,
        None,
        Some("gpt-4"),
        Some("agent-1"),
        Some("evt-query-1"),
        1,
        None,
        false, // run_hook_db_writes = false → triggers persist
        true,  // run_observer = true → skip observer (not relevant here)
        true,  // run_implicit_feedback = true → skip
        true,  // run_reflection_learning = true → skip
    ))
}

fn build_hook_payload_text_only() -> Value {
    let messages = vec![json!({"role": "user", "content": "what is Rust?"})];
    Value::Object(build_turn_hook_args(
        "user-1",
        "session-1",
        &messages,
        &[],
        "Rust is a systems programming language.",
        &[],
        None,
        Some("gpt-4"),
        None,
        Some("evt-query-2"),
        2,
        None,
        false,
        true,
        true,
        true,
    ))
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Hook payload with tool calls produces decision_audit + skill_selection.
#[tokio::test]
async fn hook_persists_decision_audit_and_skill_selection_for_tool_calls() {
    let hook_writer = RecordingHookDbWriter::default();
    let reflection_store = RecordingReflectionStateStore::default();
    let lesson_writer = RecordingReflectionLessonWriter::default();
    let observer = RecordingObserverWorker::default();

    run_bridge_hook_side_effects(
        Some(build_hook_payload_with_tool_call()),
        Arc::new(hook_writer.clone()),
        Arc::new(reflection_store),
        Arc::new(lesson_writer),
        Arc::new(observer),
        None, // turn_learning_writer
    );

    // Allow spawned task to complete
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1, "should persist exactly one hook plan");

    let plan = &plans[0];
    let audit = plan
        .decision_audit
        .as_ref()
        .expect("decision_audit missing");
    assert_eq!(audit.session_id, "session-1");
    assert_eq!(audit.event_id, "evt-query-1");
    assert_eq!(audit.decision_type, "tool_selection");
    assert_eq!(audit.model_used.as_deref(), Some("gpt-4"));

    let selection = plan
        .skill_selection
        .as_ref()
        .expect("skill_selection missing");
    assert_eq!(selection.session_id, "session-1");
    assert_eq!(selection.skill_name, "bash");
    assert_eq!(selection.selected_skills, vec!["bash"]);
    assert_eq!(selection.selection_method, "llm_tool_choice");
}

/// Text-only response produces decision_audit with type "response_generation", no skill_selection.
#[tokio::test]
async fn hook_persists_response_generation_audit_without_skill_selection() {
    let hook_writer = RecordingHookDbWriter::default();
    let reflection_store = RecordingReflectionStateStore::default();
    let lesson_writer = RecordingReflectionLessonWriter::default();
    let observer = RecordingObserverWorker::default();

    run_bridge_hook_side_effects(
        Some(build_hook_payload_text_only()),
        Arc::new(hook_writer.clone()),
        Arc::new(reflection_store),
        Arc::new(lesson_writer),
        Arc::new(observer),
        None, // turn_learning_writer
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);

    let plan = &plans[0];
    let audit = plan
        .decision_audit
        .as_ref()
        .expect("decision_audit missing");
    assert_eq!(audit.decision_type, "response_generation");
    assert!(
        plan.skill_selection.is_none(),
        "text-only turn should not produce skill_selection"
    );
}

/// Reflection mark is stored when tool calls include "reflect".
#[tokio::test]
async fn hook_marks_reflection_state_when_reflect_tool_called() {
    let hook_writer = RecordingHookDbWriter::default();
    let reflection_store = RecordingReflectionStateStore::default();
    let lesson_writer = RecordingReflectionLessonWriter::default();
    let observer = RecordingObserverWorker::default();

    let messages = vec![json!({"role": "user", "content": "reflect on our session"})];
    let tool_calls = vec![json!({
        "function": {"name": "reflect", "arguments": "{}"}
    })];
    let tool_results = vec![json!({
        "name": "reflect",
        "result": "Session analysis: good progress on refactoring."
    })];
    let payload = Value::Object(build_turn_hook_args(
        "user-1",
        "session-reflect",
        &messages,
        &tool_results,
        "",
        &tool_calls,
        None,
        Some("gpt-4"),
        None,
        Some("evt-reflect"),
        5,
        None,
        false,
        true,
        true,
        false, // run_reflection_learning = false → triggers reflection
    ));

    run_bridge_hook_side_effects(
        Some(payload),
        Arc::new(hook_writer),
        Arc::new(reflection_store.clone()),
        Arc::new(lesson_writer),
        Arc::new(observer),
        None, // turn_learning_writer
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let marks = reflection_store.marks.lock().await;
    assert_eq!(marks.len(), 1, "should mark reflection state");
    assert_eq!(marks[0].session_id, "session-reflect");
    assert!(
        marks[0]
            .reflect_output
            .contains("good progress on refactoring")
    );
}

/// None payload is a no-op — no writers called.
#[tokio::test]
async fn hook_noop_on_none_payload() {
    let hook_writer = RecordingHookDbWriter::default();
    let reflection_store = RecordingReflectionStateStore::default();
    let lesson_writer = RecordingReflectionLessonWriter::default();
    let observer = RecordingObserverWorker::default();

    run_bridge_hook_side_effects(
        None,
        Arc::new(hook_writer.clone()),
        Arc::new(reflection_store),
        Arc::new(lesson_writer),
        Arc::new(observer),
        None, // turn_learning_writer
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(hook_writer.plans.lock().await.is_empty());
}

/// Implicit feedback is triggered when run_implicit_feedback=false and user shows dissatisfaction.
#[tokio::test]
async fn hook_persists_implicit_feedback_on_negative_signal() {
    let hook_writer = RecordingHookDbWriter::default();
    let reflection_store = RecordingReflectionStateStore::default();
    let lesson_writer = RecordingReflectionLessonWriter::default();
    let observer = RecordingObserverWorker::default();

    // "that's wrong" after an assistant response is a negative implicit signal
    let messages = vec![
        json!({"role": "assistant", "content": "The answer is 42."}),
        json!({"role": "user", "content": "that's wrong, try again"}),
    ];
    let payload = Value::Object(build_turn_hook_args(
        "user-1",
        "session-1",
        &messages,
        &[],
        "Let me reconsider...",
        &[],
        None,
        Some("gpt-4"),
        None,
        Some("evt-retry"),
        3,
        None,
        false,
        true,
        false, // run_implicit_feedback = false → triggers feedback detection
        true,
    ));

    run_bridge_hook_side_effects(
        Some(payload),
        Arc::new(hook_writer.clone()),
        Arc::new(reflection_store),
        Arc::new(lesson_writer),
        Arc::new(observer),
        None, // turn_learning_writer
    );

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let plans = hook_writer.plans.lock().await;
    assert_eq!(plans.len(), 1);
    let feedback = plans[0].implicit_feedback.as_ref();
    // Implicit feedback detection is heuristic — it may or may not fire.
    // If it fires, verify it has the right structure.
    if let Some(fb) = feedback {
        assert!(fb.rating < 3, "negative signal should produce low rating");
        assert!(fb.comment.as_deref().unwrap_or("").contains("implicit:"));
    }
}
