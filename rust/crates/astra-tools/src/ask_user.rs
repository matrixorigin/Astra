use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_QUESTIONS: usize = 6;
const MIN_CHOICES: usize = 2;
const MAX_CHOICES: usize = 9;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const ASK_USER_EXAMPLE: &str = r#"{"questions":[{"header":"Scope","question":"Which scope should we ship first?","options":["Core flow","Full workflow"],"allow_freeform":true}]}"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserChoice {
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestion {
    pub header: String,
    pub question: String,
    pub options: Vec<AskUserChoice>,
    pub multi_select: bool,
    pub allow_freeform: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserPrompt {
    pub context: Option<String>,
    pub questions: Vec<AskUserQuestion>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserQuestionAnswer {
    pub question: String,
    pub answers: Vec<String>,
    pub multi_select: bool,
    pub annotation: Option<AskUserAnnotation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserAnnotation {
    pub notes: Option<String>,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskUserAnswers {
    pub answers: Vec<AskUserQuestionAnswer>,
}

impl AskUserAnswers {
    pub fn to_tool_result_value(&self) -> Value {
        let mut answers = Map::new();
        let mut annotations = Map::new();
        for answer in &self.answers {
            let value = if answer.multi_select {
                Value::Array(answer.answers.iter().cloned().map(Value::String).collect())
            } else {
                Value::String(answer.answers.first().cloned().unwrap_or_default())
            };
            answers.insert(answer.question.clone(), value);
            if let Some(annotation) = &answer.annotation {
                let mut entry = Map::new();
                if let Some(notes) = &annotation.notes {
                    entry.insert("notes".into(), Value::String(notes.clone()));
                }
                if let Some(preview) = &annotation.preview {
                    entry.insert("preview".into(), Value::String(preview.clone()));
                }
                if !entry.is_empty() {
                    annotations.insert(answer.question.clone(), Value::Object(entry));
                }
            }
        }
        let mut body = Map::new();
        body.insert("answers".into(), Value::Object(answers));
        if !annotations.is_empty() {
            body.insert("annotations".into(), Value::Object(annotations));
        }
        Value::Object(body)
    }
}

fn ask_user_contract_error(detail: impl AsRef<str>) -> String {
    format!(
        "Error: ask_user input is invalid. {}\n\
         Retry the SAME ask_user tool immediately with a top-level `questions` array. \
         Do NOT continue implementation, invent defaults, or answer as if clarification already happened.\n\
         Example: {}",
        detail.as_ref(),
        ASK_USER_EXAMPLE
    )
}

fn ask_user_response_error(detail: impl AsRef<str>) -> String {
    format!("Error: ask_user response is invalid. {}", detail.as_ref())
}

pub fn parse_ask_user_prompt(args: &Value) -> Result<AskUserPrompt, String> {
    let context = args
        .get("context")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let nested_questionnaire = extract_nested_questionnaire(args);
    let questions_value = if let Some(questions) = args.get("questions") {
        questions
    } else if let Some(nested) = nested_questionnaire.as_ref() {
        nested.get("questions").ok_or_else(|| {
            ask_user_contract_error(
                "embedded questionnaire is missing a top-level 'questions' array",
            )
        })?
    } else {
        return Err(ask_user_contract_error(
            "ask_user requires top-level 'questions': [...]. Do not send top-level 'question' or 'choices'.",
        ));
    };
    let questions = parse_questions(questions_value)?;
    let timeout_ms = parse_timeout_ms(args)?;
    Ok(AskUserPrompt {
        context,
        questions,
        timeout_ms,
    })
}

fn parse_timeout_ms(args: &Value) -> Result<Option<u64>, String> {
    let Some(value) = args.get("timeout_ms").or_else(|| args.get("timeout")) else {
        return Ok(None);
    };
    let timeout = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .ok_or_else(|| {
            ask_user_contract_error("'timeout_ms' must be an integer milliseconds value")
        })?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout) {
        return Err(ask_user_contract_error(format!(
            "'timeout_ms' must be between {MIN_TIMEOUT_MS} and {MAX_TIMEOUT_MS} milliseconds"
        )));
    }
    Ok(Some(timeout))
}

fn maybe_parse_embedded_json<'a>(value: &'a Value) -> std::borrow::Cow<'a, Value> {
    let Some(raw) = value.as_str() else {
        return std::borrow::Cow::Borrowed(value);
    };
    let trimmed = raw.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return std::borrow::Cow::Borrowed(value);
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed) => std::borrow::Cow::Owned(parsed),
        Err(_) => std::borrow::Cow::Borrowed(value),
    }
}

fn extract_nested_questionnaire(args: &Value) -> Option<Value> {
    let question_value = args.get("question")?;
    let normalized = maybe_parse_embedded_json(question_value);
    let obj = normalized.as_ref().as_object()?;
    obj.get("questions")?;
    Some(normalized.into_owned())
}

fn parse_questions(value: &Value) -> Result<Vec<AskUserQuestion>, String> {
    let normalized = maybe_parse_embedded_json(value);
    let value = normalized.as_ref();
    let items: Vec<&Value> = if let Some(items) = value.as_array() {
        items.iter().collect()
    } else if value.is_object() {
        vec![value]
    } else {
        return Err(ask_user_contract_error(
            "'questions' must be an array of question objects.",
        ));
    };
    if items.is_empty() || items.len() > MAX_QUESTIONS {
        return Err(ask_user_contract_error(format!(
            "'questions' must contain 1-{MAX_QUESTIONS} items"
        )));
    }

    let mut questions = Vec::with_capacity(items.len());
    let mut seen_headers = HashSet::new();
    let mut seen_questions = HashSet::new();
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| ask_user_contract_error("each ask_user question must be an object"))?;
        let parsed = parse_question_object(obj)?;
        let header = parsed.header.clone();
        let question = parsed.question.clone();
        if !seen_headers.insert(header.clone()) {
            return Err(ask_user_contract_error(format!(
                "duplicate ask_user header '{header}'"
            )));
        }
        if !seen_questions.insert(question.clone()) {
            return Err(ask_user_contract_error(format!(
                "duplicate ask_user question '{question}'"
            )));
        }
        questions.push(parsed);
    }

    Ok(questions)
}

fn parse_question_object(obj: &serde_json::Map<String, Value>) -> Result<AskUserQuestion, String> {
    let question = required_string(obj, "question")?;
    let header = optional_string(obj, "header").unwrap_or_else(|| derive_header(&question));
    let multi_select = obj
        .get("multi_select")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allow_freeform = obj
        .get("allow_freeform")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let options = parse_choices(obj.get("options"), allow_freeform)?;
    if !allow_freeform && !(MIN_CHOICES..=MAX_CHOICES).contains(&options.len()) {
        return Err(ask_user_contract_error(format!(
            "each ask_user question needs {MIN_CHOICES}-{MAX_CHOICES} options unless allow_freeform=true"
        )));
    }
    if options.len() > MAX_CHOICES {
        return Err(ask_user_contract_error(format!(
            "each ask_user question needs at most {MAX_CHOICES} options"
        )));
    }
    if multi_select && options.iter().any(|option| option.preview.is_some()) {
        return Err(ask_user_contract_error(
            "ask_user preview is only supported on single-select questions",
        ));
    }
    Ok(AskUserQuestion {
        header,
        question,
        options,
        multi_select,
        allow_freeform,
    })
}

fn parse_choices(
    value: Option<&Value>,
    allow_freeform: bool,
) -> Result<Vec<AskUserChoice>, String> {
    let Some(value) = value else {
        if allow_freeform {
            return Ok(Vec::new());
        }
        return Err(ask_user_contract_error(
            "ask_user questions require 'options' unless allow_freeform=true",
        ));
    };
    let normalized = maybe_parse_embedded_json(value);
    let value = normalized.as_ref();
    let items = value
        .as_array()
        .ok_or_else(|| ask_user_contract_error("'options' must be an array"))?;
    if items.is_empty() && allow_freeform {
        return Ok(Vec::new());
    }

    let mut choices = Vec::with_capacity(items.len());
    for item in items {
        let choice = if let Some(label) = item.as_str() {
            AskUserChoice {
                label: label.trim().to_string(),
                description: None,
                preview: None,
            }
        } else if let Some(obj) = item.as_object() {
            AskUserChoice {
                label: required_string(obj, "label")?,
                description: optional_string(obj, "description"),
                preview: optional_string(obj, "preview"),
            }
        } else {
            return Err(ask_user_contract_error(
                "ask_user options must be strings or {label, description} objects",
            ));
        };

        if choice.label.eq_ignore_ascii_case("other") {
            return Err(ask_user_contract_error(
                "do not include an 'Other' option; the UI adds it automatically",
            ));
        }
        if choice.label.is_empty() {
            return Err(ask_user_contract_error(
                "ask_user options must not contain empty labels",
            ));
        }
        if choices
            .iter()
            .any(|existing: &AskUserChoice| existing.label == choice.label)
        {
            return Err(ask_user_contract_error(format!(
                "duplicate ask_user option '{}'",
                choice.label
            )));
        }
        choices.push(choice);
    }

    Ok(choices)
}

fn derive_header(question: &str) -> String {
    let first_line = question.lines().next().unwrap_or(question).trim();
    let base = first_line
        .split(['?', '!', '.', ':', '\n'])
        .next()
        .unwrap_or(first_line)
        .trim();
    let mut header = if base.is_empty() {
        "Question".to_string()
    } else {
        base.to_string()
    };
    if header.is_empty() {
        header = "Question".to_string();
    }
    header
}

fn required_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            ask_user_contract_error(format!("ask_user question requires non-empty '{key}'"))
        })
}

fn optional_string(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn ask_user_outcome(ok: bool, error: Option<&str>) -> &'static str {
    if ok {
        return "submitted";
    }
    let error = error.unwrap_or_default().to_ascii_lowercase();
    if error.contains("cancelled by the user") {
        "cancelled"
    } else if error.contains("timed out") {
        "timeout"
    } else {
        "error"
    }
}

pub fn build_ask_user_prompt_telemetry(prompt: &AskUserPrompt) -> Value {
    let question_count = prompt.questions.len();
    let multi_select_count = prompt.questions.iter().filter(|q| q.multi_select).count();
    let freeform_count = prompt.questions.iter().filter(|q| q.allow_freeform).count();
    let preview_count = prompt
        .questions
        .iter()
        .filter(|q| q.options.iter().any(|option| option.preview.is_some()))
        .count();
    let headers: Vec<Value> = prompt
        .questions
        .iter()
        .map(|question| Value::String(question.header.clone()))
        .collect();
    let questions: Vec<Value> = prompt
        .questions
        .iter()
        .map(|question| {
            serde_json::json!({
                "header": question.header,
                "question": question.question,
                "option_count": question.options.len(),
                "multi_select": question.multi_select,
                "allow_freeform": question.allow_freeform,
                "has_preview": question.options.iter().any(|option| option.preview.is_some()),
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": 1,
        "question_count": question_count,
        "multi_select_count": multi_select_count,
        "freeform_count": freeform_count,
        "preview_count": preview_count,
        "context_present": prompt.context.is_some(),
        "timeout_ms": prompt.timeout_ms,
        "headers": headers,
        "questions": questions,
    })
}

fn parse_annotation_value(value: &Value) -> Option<AskUserAnnotation> {
    let obj = value.as_object()?;
    let notes = obj
        .get("notes")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
        .map(ToString::to_string);
    let preview = obj
        .get("preview")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|preview| !preview.is_empty())
        .map(ToString::to_string);
    if notes.is_none() && preview.is_none() {
        None
    } else {
        Some(AskUserAnnotation { notes, preview })
    }
}

fn parse_answers_for_prompt(prompt: &AskUserPrompt, value: &Value) -> Option<AskUserAnswers> {
    let obj = value.as_object()?;
    let answers = obj.get("answers")?.as_object()?;
    let annotations = obj.get("annotations").and_then(Value::as_object);
    let mut collected = Vec::with_capacity(prompt.questions.len());
    for question in &prompt.questions {
        let value = answers.get(&question.question)?;
        let parsed_answers = if question.multi_select {
            let parsed_answers = value
                .as_array()?
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::trim)
                        .filter(|answer| !answer.is_empty())
                })
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if parsed_answers.is_empty() {
                return None;
            }
            parsed_answers
        } else {
            let answer = value.as_str()?.trim();
            if answer.is_empty() {
                return None;
            }
            vec![answer.to_string()]
        };
        let annotation = annotations
            .and_then(|items| items.get(&question.question))
            .and_then(parse_annotation_value);
        collected.push(AskUserQuestionAnswer {
            question: question.question.clone(),
            answers: parsed_answers,
            multi_select: question.multi_select,
            annotation,
        });
    }
    Some(AskUserAnswers { answers: collected })
}

pub fn normalize_ask_user_answers(
    prompt: &AskUserPrompt,
    answers: &AskUserAnswers,
) -> Result<AskUserAnswers, String> {
    let mut by_question = HashMap::with_capacity(answers.answers.len());
    for answer in &answers.answers {
        if by_question
            .insert(answer.question.as_str(), answer)
            .is_some()
        {
            return Err(ask_user_response_error(format!(
                "duplicate answer for question '{}'",
                answer.question
            )));
        }
    }

    let mut normalized = Vec::with_capacity(prompt.questions.len());
    for question in &prompt.questions {
        let Some(answer) = by_question.remove(question.question.as_str()) else {
            return Err(ask_user_response_error(format!(
                "missing answer for question '{}'",
                question.question
            )));
        };

        let option_labels = question
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect::<HashSet<_>>();
        let mut seen_answers = HashSet::new();
        let mut normalized_answers = Vec::new();
        for raw in &answer.answers {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            if !option_labels.contains(trimmed) && !question.allow_freeform {
                return Err(ask_user_response_error(format!(
                    "answer '{}' is not valid for question '{}'",
                    trimmed, question.question
                )));
            }
            if seen_answers.insert(trimmed.to_string()) {
                normalized_answers.push(trimmed.to_string());
            }
        }
        if normalized_answers.is_empty() {
            return Err(ask_user_response_error(format!(
                "question '{}' requires at least one answer",
                question.question
            )));
        }
        if !question.multi_select && normalized_answers.len() != 1 {
            return Err(ask_user_response_error(format!(
                "question '{}' accepts exactly one answer",
                question.question
            )));
        }

        let notes = answer
            .annotation
            .as_ref()
            .and_then(|annotation| annotation.notes.as_ref())
            .map(|notes| notes.trim())
            .filter(|notes| !notes.is_empty())
            .map(ToString::to_string);
        let preview = if question.multi_select {
            None
        } else {
            normalized_answers.first().and_then(|selected| {
                question
                    .options
                    .iter()
                    .find(|option| option.label == *selected)
                    .and_then(|option| option.preview.clone())
            })
        };
        let annotation = if notes.is_none() && preview.is_none() {
            None
        } else {
            Some(AskUserAnnotation { notes, preview })
        };

        normalized.push(AskUserQuestionAnswer {
            question: question.question.clone(),
            answers: normalized_answers,
            multi_select: question.multi_select,
            annotation,
        });
    }

    if let Some(unexpected) = by_question.keys().next() {
        return Err(ask_user_response_error(format!(
            "unexpected answer for question '{}'",
            unexpected
        )));
    }

    Ok(AskUserAnswers {
        answers: normalized,
    })
}

fn ask_user_response_summary(
    prompt: &AskUserPrompt,
    outcome: &str,
    answers: Option<&AskUserAnswers>,
) -> Value {
    let mut answered_question_count = 0usize;
    let mut annotation_count = 0usize;
    let mut freeform_answer_count = 0usize;
    let mut total_answers = 0usize;
    let questions = answers
        .map(|answers| {
            answers
                .answers
                .iter()
                .map(|answer| {
                    let prompt_question = prompt
                        .questions
                        .iter()
                        .find(|question| question.question == answer.question);
                    let option_labels = prompt_question
                        .map(|question| {
                            question
                                .options
                                .iter()
                                .map(|option| option.label.as_str())
                                .collect::<HashSet<_>>()
                        })
                        .unwrap_or_default();
                    let used_freeform = answer
                        .answers
                        .iter()
                        .any(|item| !option_labels.contains(item.as_str()));
                    let notes_present = answer
                        .annotation
                        .as_ref()
                        .and_then(|annotation| annotation.notes.as_ref())
                        .is_some();
                    let preview_present = answer
                        .annotation
                        .as_ref()
                        .and_then(|annotation| annotation.preview.as_ref())
                        .is_some();
                    if !answer.answers.is_empty() {
                        answered_question_count += 1;
                    }
                    if notes_present || preview_present {
                        annotation_count += 1;
                    }
                    if used_freeform {
                        freeform_answer_count += 1;
                    }
                    total_answers += answer.answers.len();
                    serde_json::json!({
                        "question": answer.question,
                        "answers": answer.answers,
                        "answer_count": answer.answers.len(),
                        "used_freeform": used_freeform,
                        "notes_present": notes_present,
                        "preview_present": preview_present,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    serde_json::json!({
        "outcome": outcome,
        "answered_question_count": answered_question_count,
        "annotation_count": annotation_count,
        "freeform_answer_count": freeform_answer_count,
        "total_answers": total_answers,
        "questions": questions,
    })
}

pub fn build_ask_user_tool_call_audit(
    prompt: &AskUserPrompt,
    outcome: &str,
    answers: Option<&AskUserAnswers>,
    error: Option<&str>,
) -> Value {
    let mut body = Map::new();
    body.insert("prompt".into(), build_ask_user_prompt_telemetry(prompt));
    body.insert(
        "response".into(),
        ask_user_response_summary(prompt, outcome, answers),
    );
    if let Some(error) = error.map(str::trim).filter(|error| !error.is_empty()) {
        body.insert("error".into(), Value::String(error.to_string()));
    }
    Value::Object(body)
}

pub fn summarize_ask_user_tool_call(
    args_full: Option<&str>,
    result_full: Option<&str>,
    ok: bool,
    error: Option<&str>,
) -> Option<Value> {
    let args_full = args_full?;
    let args = serde_json::from_str::<Value>(args_full).ok()?;
    let prompt = parse_ask_user_prompt(&args).ok()?;
    let outcome = ask_user_outcome(ok, error);
    let answers = result_full
        .and_then(|result| serde_json::from_str::<Value>(result).ok())
        .and_then(|value| parse_answers_for_prompt(&prompt, &value));
    Some(build_ask_user_tool_call_audit(
        &prompt,
        outcome,
        answers.as_ref(),
        error,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_accepts_multi_question_prompt() {
        let prompt = parse_ask_user_prompt(&json!({
            "context": "Need a few decisions",
            "questions": [
                {
                    "header": "Frontend",
                    "question": "Which frontend stack should we use?",
                    "options": [
                        {"label": "React + TS", "description": "Flexible", "preview": "<div>React</div>"},
                        "Vue 3 + TS"
                    ]
                },
                {
                    "header": "Features",
                    "question": "Which features do you want first?",
                    "options": ["RBAC", "Reports", "Export"],
                    "multi_select": true,
                    "allow_freeform": false
                }
            ]
        }))
        .unwrap();

        assert_eq!(prompt.questions.len(), 2);
        assert_eq!(prompt.questions[0].header, "Frontend");
        assert_eq!(
            prompt.questions[0].options[0].preview.as_deref(),
            Some("<div>React</div>")
        );
        assert!(prompt.questions[1].multi_select);
        assert!(!prompt.questions[1].allow_freeform);
    }

    #[test]
    fn parse_accepts_bounded_timeout_ms() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [{
                "header": "Scope",
                "question": "Which scope?",
                "options": ["Core", "Full"]
            }],
            "timeout_ms": "5000"
        }))
        .unwrap();

        assert_eq!(prompt.timeout_ms, Some(5000));
        let telemetry = build_ask_user_prompt_telemetry(&prompt);
        assert_eq!(telemetry["timeout_ms"], json!(5000));
    }

    #[test]
    fn parse_accepts_timeout_ms_at_inclusive_bounds() {
        for timeout in [MIN_TIMEOUT_MS, MAX_TIMEOUT_MS] {
            let prompt = parse_ask_user_prompt(&json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which scope?",
                    "options": ["Core", "Full"]
                }],
                "timeout_ms": timeout
            }))
            .unwrap();
            assert_eq!(prompt.timeout_ms, Some(timeout), "{timeout}");
        }
    }

    #[test]
    fn parse_rejects_unbounded_or_invalid_timeout() {
        for timeout in [json!(999), json!(3_600_001), json!("soon")] {
            let err = parse_ask_user_prompt(&json!({
                "questions": [{
                    "header": "Scope",
                    "question": "Which scope?",
                    "options": ["Core", "Full"]
                }],
                "timeout_ms": timeout
            }))
            .unwrap_err();
            assert!(err.contains("timeout_ms"), "{err}");
        }
    }

    #[test]
    fn parse_rejects_missing_questions() {
        let err = parse_ask_user_prompt(&json!({"context": "missing"})).unwrap_err();
        assert!(err.contains("top-level 'questions'"));
        assert!(err.contains("Do not send top-level 'question' or 'choices'"));
        assert!(err.contains("Retry the SAME ask_user tool immediately"));
    }

    #[test]
    fn parse_rejects_legacy_top_level_shape() {
        let err = parse_ask_user_prompt(&json!({
            "question": "Which stack should we use?",
            "choices": ["Rust", "TypeScript"]
        }))
        .unwrap_err();

        assert!(err.contains("top-level 'questions'"));
        assert!(err.contains("Do not send top-level 'question' or 'choices'"));
        assert!(err.contains("Do NOT continue implementation"));
    }

    #[test]
    fn parse_rejects_manual_other_option() {
        let err = parse_ask_user_prompt(&json!({
            "questions": [{
                "header": "Stack",
                "question": "Pick one",
                "options": ["React", "Other"]
            }]
        }))
        .unwrap_err();

        assert!(err.contains("UI adds it automatically"));
    }

    #[test]
    fn parse_rejects_preview_on_multi_select_question() {
        let err = parse_ask_user_prompt(&json!({
            "questions": [{
                "header": "Compare",
                "question": "Pick many",
                "options": [
                    {"label": "A", "preview": "preview a"},
                    {"label": "B", "preview": "preview b"}
                ],
                "multi_select": true
            }]
        }))
        .unwrap_err();

        assert!(err.contains("single-select"));
    }

    #[test]
    fn parse_accepts_freeform_only_questionnaire_shape() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [{
                "question": "What should we name this command?"
            }]
        }))
        .unwrap();

        assert_eq!(prompt.questions.len(), 1);
        assert_eq!(
            prompt.questions[0].header,
            "What should we name this command"
        );
        assert!(prompt.questions[0].options.is_empty());
        assert!(prompt.questions[0].allow_freeform);
    }

    #[test]
    fn parse_accepts_singleton_question_object_for_questions() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": {
                "header": "Scope",
                "question": "Which scope should we ship first?",
                "options": ["Core flow", "Full workflow"]
            }
        }))
        .unwrap();

        assert_eq!(prompt.questions.len(), 1);
        assert_eq!(prompt.questions[0].header, "Scope");
        assert_eq!(prompt.questions[0].options.len(), 2);
    }

    #[test]
    fn parse_accepts_stringified_singleton_question_object() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": "{\"header\":\"Scope\",\"question\":\"Which scope should we ship first?\",\"options\":[\"Core flow\",\"Full workflow\"]}"
        }))
        .unwrap();

        assert_eq!(prompt.questions.len(), 1);
        assert_eq!(prompt.questions[0].header, "Scope");
    }

    #[test]
    fn parse_accepts_nested_questionnaire_string_in_top_level_question() {
        let prompt = parse_ask_user_prompt(&json!({
            "question": "{\"questions\":[{\"header\":\"技术栈选择\",\"question\":\"您希望使用什么技术栈？\",\"options\":[\"Python\",\"Node.js\"],\"allow_freeform\":true}]}"
        }))
        .unwrap();

        assert_eq!(prompt.questions.len(), 1);
        assert_eq!(prompt.questions[0].header, "技术栈选择");
        assert_eq!(prompt.questions[0].question, "您希望使用什么技术栈？");
    }

    #[test]
    fn parse_derives_header_when_question_item_omits_it() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [{
                "question": "Which database should we use?",
                "options": ["PostgreSQL", "SQLite"]
            }]
        }))
        .unwrap();

        assert_eq!(prompt.questions[0].header, "Which database should we use");
    }

    #[test]
    fn tool_result_includes_annotations_when_present() {
        let value = AskUserAnswers {
            answers: vec![AskUserQuestionAnswer {
                question: "Which layout?".into(),
                answers: vec!["Cards".into()],
                multi_select: false,
                annotation: Some(AskUserAnnotation {
                    notes: Some("ship it".into()),
                    preview: Some("card-a".into()),
                }),
            }],
        }
        .to_tool_result_value();

        assert_eq!(
            value,
            json!({
                "answers": {"Which layout?": "Cards"},
                "annotations": {"Which layout?": {"notes": "ship it", "preview": "card-a"}}
            })
        );
    }

    #[test]
    fn tool_call_audit_from_raw_marks_cancelled_outcome() {
        let audit = summarize_ask_user_tool_call(
            Some(
                r#"{"questions":[{"header":"Scope","question":"Which scope should we ship first?","options":["Core flow","Full workflow"],"allow_freeform":true}]}"#,
            ),
            None,
            false,
            Some("Error: ask_user was cancelled by the user"),
        )
        .expect("audit");

        assert_eq!(audit["prompt"]["question_count"], 1);
        assert_eq!(audit["prompt"]["headers"][0], "Scope");
        assert_eq!(audit["response"]["outcome"], "cancelled");
        assert_eq!(audit["response"]["answered_question_count"], 0);
        assert_eq!(audit["error"], "Error: ask_user was cancelled by the user");
    }

    #[test]
    fn tool_call_audit_from_raw_tracks_submitted_annotations_and_freeform() {
        let audit = summarize_ask_user_tool_call(
            Some(
                r#"{"questions":[{"header":"Layout","question":"Which layout should we ship?","options":[{"label":"Cards","preview":"cards-preview"},{"label":"Table","preview":"table-preview"}],"allow_freeform":false},{"header":"Notes","question":"What should we name this mode?","allow_freeform":true}]}"#,
            ),
            Some(
                r#"{"answers":{"Which layout should we ship?":"Cards","What should we name this mode?":"Zen mode"},"annotations":{"Which layout should we ship?":{"notes":"ship cards first","preview":"cards-preview"}}}"#,
            ),
            true,
            None,
        )
        .expect("audit");

        assert_eq!(audit["prompt"]["question_count"], 2);
        assert_eq!(audit["prompt"]["preview_count"], 1);
        assert_eq!(audit["response"]["outcome"], "submitted");
        assert_eq!(audit["response"]["answered_question_count"], 2);
        assert_eq!(audit["response"]["annotation_count"], 1);
        assert_eq!(audit["response"]["freeform_answer_count"], 1);
        assert_eq!(
            audit["response"]["questions"][1]["used_freeform"],
            serde_json::Value::Bool(true)
        );
    }

    #[test]
    fn normalize_answers_preserves_prompt_order_and_preview() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [
                {
                    "header": "Layout",
                    "question": "Which layout should we ship?",
                    "options": [
                        {"label": "Cards", "preview": "cards-preview"},
                        {"label": "Table", "preview": "table-preview"}
                    ],
                    "allow_freeform": false
                },
                {
                    "header": "Features",
                    "question": "Which features should we include first?",
                    "options": ["RBAC", "Reports"],
                    "multi_select": true,
                    "allow_freeform": true
                }
            ]
        }))
        .unwrap();

        let normalized = normalize_ask_user_answers(
            &prompt,
            &AskUserAnswers {
                answers: vec![
                    AskUserQuestionAnswer {
                        question: "Which features should we include first?".into(),
                        answers: vec!["Reports".into(), "Custom".into(), "Reports".into()],
                        multi_select: false,
                        annotation: Some(AskUserAnnotation {
                            notes: Some("keep this".into()),
                            preview: Some("ignored".into()),
                        }),
                    },
                    AskUserQuestionAnswer {
                        question: "Which layout should we ship?".into(),
                        answers: vec!["Cards".into()],
                        multi_select: true,
                        annotation: Some(AskUserAnnotation {
                            notes: Some("ship it".into()),
                            preview: Some("ignored".into()),
                        }),
                    },
                ],
            },
        )
        .unwrap();

        assert_eq!(
            normalized.answers[0].question,
            "Which layout should we ship?"
        );
        assert_eq!(normalized.answers[0].answers, vec!["Cards"]);
        assert_eq!(
            normalized.answers[0]
                .annotation
                .as_ref()
                .and_then(|annotation| annotation.preview.as_deref()),
            Some("cards-preview")
        );
        assert_eq!(
            normalized.answers[0]
                .annotation
                .as_ref()
                .and_then(|annotation| annotation.notes.as_deref()),
            Some("ship it")
        );
        assert_eq!(
            normalized.answers[1].question,
            "Which features should we include first?"
        );
        assert_eq!(normalized.answers[1].answers, vec!["Reports", "Custom"]);
        assert_eq!(
            normalized.answers[1]
                .annotation
                .as_ref()
                .and_then(|annotation| annotation.notes.as_deref()),
            Some("keep this")
        );
        assert_eq!(
            normalized.answers[1]
                .annotation
                .as_ref()
                .and_then(|annotation| annotation.preview.as_deref()),
            None
        );
    }

    #[test]
    fn normalize_answers_rejects_missing_question() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [
                {
                    "header": "Scope",
                    "question": "Which scope should we ship first?",
                    "options": ["Core flow", "Full workflow"]
                },
                {
                    "header": "Name",
                    "question": "What should we call it?",
                    "allow_freeform": true
                }
            ]
        }))
        .unwrap();

        let err = normalize_ask_user_answers(
            &prompt,
            &AskUserAnswers {
                answers: vec![AskUserQuestionAnswer {
                    question: "Which scope should we ship first?".into(),
                    answers: vec!["Core flow".into()],
                    multi_select: false,
                    annotation: None,
                }],
            },
        )
        .unwrap_err();

        assert!(err.contains("missing answer for question 'What should we call it?'"));
    }

    #[test]
    fn parse_answers_rejects_empty_multiselect_after_trimming() {
        let prompt = parse_ask_user_prompt(&json!({
            "questions": [{
                "header": "Features",
                "question": "Which features should we include first?",
                "options": ["RBAC", "Reports"],
                "multi_select": true
            }]
        }))
        .unwrap();

        let parsed = parse_answers_for_prompt(
            &prompt,
            &json!({
                "answers": {
                    "Which features should we include first?": [" ", "\t"]
                }
            }),
        );

        assert!(parsed.is_none());
    }
}
