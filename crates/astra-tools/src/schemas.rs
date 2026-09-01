//! Tool schema definitions for all edge tools.
//
//! Each schema is a JSON object following the OpenAI function-calling format:
//! `{ "type": "function", "function": { "name": ..., "description": ..., "parameters": ... } }`

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

pub const PER_ACTION_REQUIRED_KEY: &str = "x-astra-per-action-required";
pub const PER_ACTION_ANY_OF_REQUIRED_KEY: &str = "x-astra-per-action-any-of-required";
pub const PER_ACTION_ALLOWED_KEY: &str = "x-astra-per-action-allowed";
pub const ACTION_SURFACES_KEY: &str = "x-astra-action-surfaces";
pub const SURFACE_DESCRIPTIONS_KEY: &str = "x-astra-surface-descriptions";
pub const SURFACE_DISCOVERY_SUMMARIES_KEY: &str = "x-astra-surface-discovery-summaries";

/// Structured failure returned when model-authored arguments do not satisfy
/// the invocation constraints encoded in the advertised built-in schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArgumentValidationError {
    pub tool_name: String,
    pub action: Option<String>,
    pub issues: Vec<String>,
    malformed_parse_error: Option<Value>,
}

impl fmt::Display for ToolArgumentValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Invalid arguments for tool `{}`", self.tool_name)?;
        if let Some(action) = self.action.as_deref() {
            write!(formatter, " (action `{action}`)")?;
        }
        write!(formatter, ": {}", self.issues.join("; "))
    }
}

impl std::error::Error for ToolArgumentValidationError {}

impl ToolArgumentValidationError {
    #[must_use]
    pub fn output(&self) -> String {
        if let Some(parse_error) = self.malformed_parse_error.as_ref() {
            let mut body = json!({
                "status": "failed",
                "error_kind": astra_core::ErrorKind::ToolInvalidArgs.as_str(),
                "error": "Tool arguments were not valid JSON; the tool was not executed.",
                "advisory": {
                    "kind": "malformed_tool_arguments",
                    "tool": self.tool_name,
                    "executed": false,
                    "next_step": "Retry the same native tool once with one complete JSON argument object matching the advertised schema.",
                },
            });
            let mut metadata = Map::new();
            if let Some(kind @ ("invalid_json" | "truncated")) =
                parse_error.get("kind").and_then(Value::as_str)
            {
                metadata.insert("kind".into(), json!(kind));
            }
            if let Some(category @ ("io" | "syntax" | "data" | "eof")) =
                parse_error.get("category").and_then(Value::as_str)
            {
                metadata.insert("category".into(), json!(category));
            }
            for field in ["argument_bytes", "line", "column"] {
                if let Some(value) = parse_error.get(field).and_then(Value::as_u64) {
                    metadata.insert(field.into(), json!(value));
                }
            }
            if !metadata.is_empty() {
                body["advisory"]["parse_error"] = Value::Object(metadata);
            }
            return body.to_string();
        }
        format!(
            "Error: {self}. Correct the arguments and issue one new call matching the advertised schema."
        )
    }

    #[must_use]
    pub fn failure_evidence(&self) -> astra_core::ToolFailureEvidence {
        astra_core::ToolFailureEvidence::new(
            astra_core::ErrorKind::ToolInvalidArgs,
            astra_core::ToolFailureCause::InvalidArguments,
            false,
            vec![astra_core::ToolRecoveryAction::CorrectArguments],
        )
    }

    #[must_use]
    pub fn into_tool_result(self) -> crate::ToolResult {
        let evidence = self.failure_evidence();
        crate::ToolResult::error(self.output()).with_failure_evidence(evidence)
    }
}

fn built_in_schema_index() -> &'static HashMap<String, Value> {
    static INDEX: OnceLock<HashMap<String, Value>> = OnceLock::new();
    INDEX.get_or_init(|| {
        all_tool_schemas()
            .into_iter()
            .filter_map(|schema| {
                let name = schema.get("function")?.get("name")?.as_str()?.to_string();
                Some((name, schema))
            })
            .collect()
    })
}

fn value_is_present(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::String(value)) => !value.trim().is_empty(),
        // JSON Schema `required` constrains field presence, not array
        // cardinality. Array emptiness is governed by the field's `minItems`
        // contract; conflating the two rejects legitimate required `[]`
        // values such as a dependency-free graph.
        Some(Value::Array(_)) => true,
        Some(_) => true,
    }
}

fn value_satisfies_required_alternative(
    parameters: &Map<String, Value>,
    arguments: &Map<String, Value>,
    field: &str,
) -> bool {
    let value = arguments.get(field);
    if !value_is_present(value) {
        return false;
    }
    let Some(values) = value.and_then(Value::as_array) else {
        return true;
    };
    let minimum = parameters
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(field))
        .and_then(|schema| schema.get("minItems"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    values.len() >= minimum as usize
}

fn schema_type_matches(value: &Value, expected: &Value) -> bool {
    let matches_one = |expected: &str| match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
    };
    match expected {
        Value::String(expected) => matches_one(expected),
        Value::Array(expected) => expected.iter().filter_map(Value::as_str).any(matches_one),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn collect_required_fields(parameters: &Map<String, Value>, action: Option<&str>) -> Vec<String> {
    let mut required = parameters
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    if let Some(action) = action
        && let Some(fields) = parameters
            .get(PER_ACTION_REQUIRED_KEY)
            .and_then(Value::as_object)
            .and_then(|requirements| requirements.get(action))
            .and_then(Value::as_array)
    {
        required.extend(fields.iter().filter_map(Value::as_str).map(str::to_string));
    }
    required.sort();
    required.dedup();
    required
}

fn any_of_required_alternatives(
    parameters: &Map<String, Value>,
    action: Option<&str>,
) -> Vec<Vec<String>> {
    let Some(action) = action else {
        return Vec::new();
    };
    parameters
        .get(PER_ACTION_ANY_OF_REQUIRED_KEY)
        .and_then(Value::as_object)
        .and_then(|requirements| requirements.get(action))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|fields| !fields.is_empty())
        .collect()
}

fn field_path(parent: &str, field: &str) -> String {
    if parent.is_empty() {
        format!("field `{field}`")
    } else {
        format!("{parent} field `{field}`")
    }
}

fn validate_schema_value(
    value: &Value,
    schema: &Value,
    path: &str,
    check_required: bool,
    issues: &mut Vec<String>,
) {
    if let Some(alternatives) = schema.get("anyOf").and_then(Value::as_array) {
        let compatible = alternatives
            .iter()
            .filter(|candidate| {
                candidate
                    .get("type")
                    .is_none_or(|expected| schema_type_matches(value, expected))
            })
            .collect::<Vec<_>>();
        let candidates = if compatible.is_empty() {
            alternatives.iter().collect::<Vec<_>>()
        } else {
            compatible
        };
        let mut best_failure: Option<Vec<String>> = None;
        for candidate in candidates {
            let mut candidate_issues = Vec::new();
            validate_schema_value(
                value,
                candidate,
                path,
                check_required,
                &mut candidate_issues,
            );
            if candidate_issues.is_empty() {
                best_failure = None;
                break;
            }
            if best_failure
                .as_ref()
                .is_none_or(|best| candidate_issues.len() < best.len())
            {
                best_failure = Some(candidate_issues);
            }
        }
        if let Some(best_failure) = best_failure {
            issues.extend(best_failure);
            return;
        }
    }

    if let Some(expected) = schema.get("type")
        && !schema_type_matches(value, expected)
    {
        issues.push(format!(
            "{path} has type {}, expected {expected}",
            json_type_name(value)
        ));
        return;
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        issues.push(format!("{path} is outside its advertised enum"));
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
            && (length < minimum || (minimum > 0 && text.trim().chars().count() < minimum as usize))
        {
            issues.push(format!("{path} requires at least {minimum} character(s)"));
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64)
            && length > maximum
        {
            issues.push(format!("{path} accepts at most {maximum} character(s)"));
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
            && number < minimum
        {
            issues.push(format!("{path} must be at least {minimum}"));
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
            && number > maximum
        {
            issues.push(format!("{path} must be at most {maximum}"));
        }
    }

    if let Some(values) = value.as_array() {
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && values.len() < minimum as usize
        {
            issues.push(format!("{path} requires at least {minimum} item(s)"));
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
            && values.len() > maximum as usize
        {
            issues.push(format!("{path} accepts at most {maximum} item(s)"));
        }
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in values.iter().enumerate() {
                let item_path = format!("{path} item {index}");
                validate_schema_value(item, item_schema, &item_path, true, issues);
                if item.as_str().is_some_and(|item| item.trim().is_empty()) {
                    issues.push(format!("{item_path} must be non-empty"));
                }
            }
        }
    }

    let Some(object) = value.as_object() else {
        return;
    };
    if check_required && let Some(required) = schema.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !value_is_present(object.get(field)) {
                let prefix = if path.is_empty() {
                    String::new()
                } else {
                    format!("{path} ")
                };
                issues.push(format!(
                    "{prefix}missing non-empty required field `{field}`"
                ));
            }
        }
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false)
        && let Some(properties) = properties
    {
        let mut unknown = object
            .keys()
            .filter(|field| !properties.contains_key(*field))
            .cloned()
            .collect::<Vec<_>>();
        unknown.sort();
        if !unknown.is_empty() {
            let label = if path.is_empty() {
                "unknown field(s)".to_string()
            } else {
                format!("{path} has unknown field(s)")
            };
            issues.push(format!("{label}: {}", unknown.join(", ")));
        }
    }
    if let Some(properties) = properties {
        for (field, child) in object {
            if let Some(child_schema) = properties.get(field) {
                validate_schema_value(child, child_schema, &field_path(path, field), true, issues);
            }
        }
    }
}

/// Validate invocation-level constraints from the canonical built-in schema.
///
/// Unknown/dynamic tools are intentionally left to their owning provider.
/// Built-ins use this at every executor boundary, so CLI, server-only, and
/// edge+server deployments cannot drift into handler-specific validation.
pub fn validate_tool_arguments(
    tool_name: &str,
    args: &Value,
) -> Result<(), ToolArgumentValidationError> {
    let Some(schema) = built_in_schema_index().get(tool_name) else {
        return Ok(());
    };
    let Some(parameters) = schema
        .get("function")
        .and_then(|function| function.get("parameters"))
        .and_then(Value::as_object)
    else {
        return Ok(());
    };
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|action| !action.is_empty());
    let mut issues = Vec::new();
    let Some(arguments) = args.as_object() else {
        issues.push(format!(
            "expected an object, received {}",
            json_type_name(args)
        ));
        return Err(ToolArgumentValidationError {
            tool_name: tool_name.to_string(),
            action: action.map(str::to_string),
            issues,
            malformed_parse_error: None,
        });
    };

    // A provider preserves undecodable arguments as this sentinel. Handle it
    // before ordinary schema checks so the original parse fact remains a
    // typed, machine-readable "not executed" receipt.
    if let Some(parse_error) = arguments.get("_parse_error") {
        return Err(ToolArgumentValidationError {
            tool_name: tool_name.to_string(),
            action: None,
            issues: vec!["arguments were not valid JSON".to_string()],
            malformed_parse_error: Some(parse_error.clone()),
        });
    }

    for field in collect_required_fields(parameters, action) {
        if !value_is_present(arguments.get(&field)) {
            issues.push(format!("missing non-empty required field `{field}`"));
        }
    }

    let alternatives = any_of_required_alternatives(parameters, action);
    if !alternatives.is_empty()
        && !alternatives.iter().any(|fields| {
            fields
                .iter()
                .all(|field| value_satisfies_required_alternative(parameters, arguments, field))
        })
    {
        let rendered = alternatives
            .iter()
            .map(|fields| fields.join(" + "))
            .collect::<Vec<_>>()
            .join(" or ");
        issues.push(format!("requires one of: {rendered}"));
    }

    if let Some(action) = action
        && let Some(allowed) = parameters
            .get(PER_ACTION_ALLOWED_KEY)
            .and_then(Value::as_object)
            .and_then(|allowed| allowed.get(action))
            .and_then(Value::as_array)
    {
        let allowed = allowed.iter().filter_map(Value::as_str).collect::<Vec<_>>();
        let mut disallowed = arguments
            .keys()
            .filter(|field| !allowed.contains(&field.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        disallowed.sort();
        if !disallowed.is_empty() {
            issues.push(format!(
                "field(s) not allowed for action `{action}`: {}",
                disallowed.join(", ")
            ));
        }
    }

    validate_schema_value(
        args,
        &Value::Object(parameters.clone()),
        "",
        false,
        &mut issues,
    );

    if issues.is_empty() {
        Ok(())
    } else {
        Err(ToolArgumentValidationError {
            tool_name: tool_name.to_string(),
            action: action.map(str::to_string),
            issues,
            malformed_parse_error: None,
        })
    }
}

/// RPC tools exposed inside server-side `run_script`.
///
/// This is intentionally narrower than [`crate::run_script::RPC_ALLOWED_TOOLS`]:
/// the web/API server must only advertise sub-tools that the
/// `ServerToolExecutor` can actually route in-process.
pub const SERVER_RUN_SCRIPT_RPC_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "list_dir",
    "grep",
    "web_fetch",
    "bash",
];

fn start_work_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "start_work",
            "description": "Establish one canonical Work and its initial ordered task list around the current durable conversation. Use this when the user's goal has multiple independently accepted outcomes or should continue as durable work; decide this from the goal's semantics without requiring the user to request a plan or use a command. Count user acceptance units, not response containers: explicitly requested A and B remain independent even when one final message presents both when each owes its own payload or evidence and either remains useful if the other fails; inputs used only for one combined conclusion are one outcome. An explicit same-turn multi-agent request without tracked lifecycle uses agent_fanout instead of Work. When a canonical Work already exists, use this same typed entrypoint for a follow-up task list; the server extends that branch without creating a second Work or asking the model to author graph identities. Do not use it for a simple question or one-shot response. Set activation=start for ordinary work that should proceed now. Set activation=defer only when the user explicitly wants a visible plan without execution yet. This lifecycle action declares work; it cannot claim completion. Supply the smallest useful sequence of independently executable, evidence-producing outcomes only: task identity, ordering, and execution dependencies are assigned by the server. Preserve staged chronology: when the user says an item should be added, discovered, or decided after a later event, omit it from this initial list and use the typed graph-update path only after that event occurs. If one bounded retrieval, inspection, or mutation produces all requested evidence, keep it as one task; do not split acquisition, extraction, and reporting of the same outcome into separate tasks. Remove any item whose sole expected result is to summarize, format, combine, report, or restate evidence from other items. Preserve explicitly named execution tracks one-for-one: N tracks means exactly N tasks unless the user changes scope. For example, `investigate A, investigate B, then answer` means two tasks (A and B), not a third answer task. The final response is outside the task list. A successful start result normally includes initial_task, the first durable primary-session assignment; execute it directly instead of spending another model round on run_next_work_item. Do not immediately revise a successful initial graph merely to rephrase it; revision requires new user guidance or newly observed evidence that materially changes scope, order, feasibility, or completion.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "goal": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 16384,
                        "description": "Concise outcome-oriented goal preserving the user's explicit constraints."
                    },
                    "activation": {
                        "type": "string",
                        "enum": ["start", "defer"],
                        "description": "Whether to atomically assign the first task now, or leave the task list durably ready without creating an execution attempt."
                    },
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 8,
                        "description": "Small ordered list of evidence-producing outcomes executable at initial admission. Keep only meaningful independently verifiable units; this is not a transcript checklist. Omit tasks the user explicitly stages for later addition or discovery, and omit final synthesis/reporting items that merely combine other task evidence. Each task must have a narrow objective and an expected result that can end its attempt as soon as sufficient evidence exists. List order is the default primary-session execution order; the server owns IDs and dependency mechanics.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "objective": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "expected_result": {"type": "string", "minLength": 1, "maxLength": 8192}
                            },
                            "required": ["objective", "expected_result"]
                        }
                    }
                },
                "required": ["goal", "activation", "tasks"]
            }
        }
    })
}

fn run_next_work_item_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "run_next_work_item",
            "description": "Select and bind one next foreground canonical Work task only when no assignment was returned by start_work or settle_work_item. The server, not the model, selects the dependency-ready task and derives its immutable attempt and settlement authority. When start_work returns initial_task or settlement returns next_task, execute that assignment directly instead of calling this tool. The returned expected_result is the attempt's completion boundary: gather sufficient direct evidence, settle immediately once it is satisfied, and do not broaden into adjacent investigation. Create a child agent only for a real isolation or parallelism boundary, never merely because a Work task exists.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }
        }
    })
}

fn settle_work_item_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "settle_work_item",
            "description": "Report the typed delivery outcome for the exact canonical WorkItem attempt assigned to this run. Call exactly once after attempting the task and before the final response. Runtime completion is not delivery: use delivered only after a literal gap check proves that direct evidence contains every payload and verification field in expected_result. Every explicit conjunct, including a named behavior check, command, test, or observable workflow, requires direct successful evidence; an unrun or failed check remains a gap, and compilation, imports, or adjacent smoke checks do not substitute for it. A reachable/index/home page, category list, or successful action does not substitute for a requested item, value, article, result, or source. If any required field is absent, continue the focused evidence path; use blocked with a structured blocker when the dependency/capability is unavailable, or failed when execution itself failed. None of these outcomes means cancelled: a requested cancellation is a canonical graph revision with declaration_state=cancelled through the inspect/propose path, never a word in this summary. The summary is a derived progress note, not an authoritative evidence source: include every required observed payload field, copy exact values faithfully, identify direct tool/artifact sources when material, and never replace conflicting direct evidence with the summary. The server derives Work/item/attempt identity from the trusted current run. A successful result may atomically include next_task; when present, execute that assignment directly instead of calling run_next_work_item again.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "outcome": {"type": "string", "enum": ["delivered", "blocked", "failed"]},
                    "summary": {"type": "string", "minLength": 1, "maxLength": 8192},
                    "blocker_kind": {
                        "type": "string",
                        "enum": ["capability_unavailable", "dependency_blocked", "policy_blocked", "external_unavailable"]
                    },
                    "unavailable_capabilities": {
                        "type": "array",
                        "maxItems": 16,
                        "items": {"type": "string", "minLength": 1, "maxLength": 128}
                    }
                },
                "required": ["outcome", "summary"]
            }
        }
    })
}

fn inspect_work_plan_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "inspect_work_plan",
            "description": "Read one bounded page of the content-addressed canonical Work planning context and its pinned observation fact, cause, and evidence references. Follow next_offset values with the same context_id to inspect larger plans; a changed context fails stale. Inspect before proposing any graph change; context_id is the exact optimistic-concurrency basis for propose_work_plan.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "context_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 96,
                        "description": "Omit on the first page; use the exact returned context_id on later pages."
                    },
                    "item_offset": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 256
                    },
                    "dependency_offset": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 1024
                    }
                },
                "required": []
            }
        }
    })
}

fn propose_work_plan_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "propose_work_plan",
            "description": "Persist a non-authoritative, revision-pinned Task Graph patch against an exact inspected Work context. Use it to keep the canonical graph current when execution evidence or the user's guidance changes scope, sequencing, or what should stop. Item identity is semantic: use an active successor revision only when the same durable unit of work continues; when work is retired or replaced, give the old item a cancelled or superseded revision and add the replacement under a fresh item_id. Retirement preserves execution and evidence history. A patch may also add or remove dependencies. Small purely additive patches may proceed without interruption; revisions and removals use the normal typed approval path. Preserve prior item text when only changing declaration_state, explain why the graph changed, trust the returned status, and never claim a pending proposal changed the accepted plan.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "context_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 96,
                        "description": "Exact context_id returned by inspect_work_plan."
                    },
                    "reason": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 512,
                        "description": "Concise fact-based reason for this graph change."
                    },
                    "additions": {
                        "type": "array",
                        "maxItems": 64,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "item_id": {"type": "string", "minLength": 1, "maxLength": 64, "description": "Fresh identity not present in the inspected graph. Never reuse the identity of an item being revised or retired in this patch."},
                                "kind": {"type": "string", "enum": ["milestone", "task"]},
                                "objective": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "expected_result": {"type": "string", "minLength": 1, "maxLength": 8192}
                            },
                            "required": ["item_id", "kind", "objective", "expected_result"]
                        }
                    },
                    "revisions": {
                        "type": "array",
                        "maxItems": 64,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "item_id": {"type": "string", "minLength": 1, "maxLength": 64},
                                "expected_revision": {"type": "integer", "minimum": 1},
                                "kind": {"type": "string", "enum": ["milestone", "task"]},
                                "objective": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "expected_result": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "declaration_state": {"type": "string", "enum": ["active", "superseded", "cancelled"], "description": "Use active only when the same semantic work item continues. Use superseded or cancelled to retire the old identity when replacement work receives a fresh addition identity."}
                            },
                            "required": ["item_id", "expected_revision", "kind", "objective", "expected_result", "declaration_state"]
                        }
                    },
                    "dependencies": {
                        "type": "array",
                        "maxItems": 256,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "predecessor_item_id": {"type": "string", "minLength": 1, "maxLength": 64},
                                "successor_item_id": {"type": "string", "minLength": 1, "maxLength": 64}
                            },
                            "required": ["predecessor_item_id", "successor_item_id"]
                        }
                    },
                    "dependency_removals": {
                        "type": "array",
                        "maxItems": 256,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "predecessor_item_id": {"type": "string", "minLength": 1, "maxLength": 64},
                                "successor_item_id": {"type": "string", "minLength": 1, "maxLength": 64}
                            },
                            "required": ["predecessor_item_id", "successor_item_id"]
                        }
                    }
                },
                "required": ["context_id", "reason", "additions", "revisions", "dependencies", "dependency_removals"]
            }
        }
    })
}

fn inspect_work_criteria_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "inspect_work_criteria",
            "description": "Read one bounded page of the accepted Done-when criteria for the canonical Work branch bound to this session. The returned context_id pins Work, Goal, criterion-set, branch, and graph revisions. Follow next_offset with that exact context_id; inspect every page before proposing a complete replacement set.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "context_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 96,
                        "description": "Omit on the first page; use the exact returned context_id on continuation pages."
                    },
                    "offset": {"type": "integer", "minimum": 0, "maximum": 128}
                },
                "required": []
            }
        }
    })
}

fn proposed_criterion_definition_schema() -> Value {
    json!({
        "anyOf": [
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["command_check"]},
                    "statement": {"type": "string", "minLength": 1, "maxLength": 16384},
                    "command": {"type": "string", "minLength": 1, "maxLength": 65536}
                },
                "required": ["kind", "statement", "command"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["test_check"]},
                    "statement": {"type": "string", "minLength": 1, "maxLength": 16384},
                    "command": {"type": "string", "minLength": 1, "maxLength": 65536}
                },
                "required": ["kind", "statement", "command"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "kind": {"type": "string", "enum": ["human_review"]},
                    "statement": {"type": "string", "minLength": 1, "maxLength": 16384}
                },
                "required": ["kind", "statement"]
            }
        ]
    })
}

fn propose_work_criteria_schema() -> Value {
    let definition = proposed_criterion_definition_schema();
    json!({
        "type": "function",
        "function": {
            "name": "propose_work_criteria",
            "description": "Persist a non-authoritative complete Done-when criterion-set proposal against one exact inspected Work context. Include every accepted existing member that should remain plus explicit new definitions. This tool never accepts its own proposal: trust the returned pending status and continue useful work without repeatedly asking; the user reviews it through the Work surface.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "context_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 96,
                        "description": "Exact context_id returned by inspect_work_criteria."
                    },
                    "members": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 128,
                        "items": {
                            "anyOf": [
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "member_kind": {"type": "string", "enum": ["existing"]},
                                        "criterion_id": {"type": "string", "minLength": 1, "maxLength": 64},
                                        "revision": {"type": "integer", "minimum": 1}
                                    },
                                    "required": ["member_kind", "criterion_id", "revision"]
                                },
                                {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "member_kind": {"type": "string", "enum": ["new"]},
                                        "criterion_id": {"type": "string", "minLength": 1, "maxLength": 64},
                                        "definition": definition
                                    },
                                    "required": ["member_kind", "criterion_id", "definition"]
                                }
                            ]
                        }
                    }
                },
                "required": ["context_id", "members"]
            }
        }
    })
}

pub fn all_tool_schemas() -> Vec<Value> {
    let mut schemas = all_tool_schemas_core();
    // run_script is Unix-only (UDS RPC transport). Always exposed on Unix;
    // there is no environment gate for production tools.
    #[cfg(unix)]
    {
        schemas.push(run_script_schema_default());
    }
    schemas.push(json!({
        "type": "function",
        "function": {
            "name": "powershell",
            "description": "Execute a PowerShell command. Use for Windows shell tasks, pwsh scripts, and cross-platform automation when PowerShell syntax is preferred over bash. PREFER dedicated tools (git, glob, grep, read_file, write_file, str_replace) over shell commands when they cover the operation.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "PowerShell command to run"},
                    "timeout": {"type": "number", "default": 120, "description": "Timeout in seconds. Pass a larger value for long-running builds/tests (e.g. 300 for cargo build, 600 for full test suites)."}
                },
                "required": ["command"]
            }
        }
    }));
    schemas
}

/// Add the managed environment-lifetime Bash contract only for an executor
/// that actually implements it. Shared/server executors remain foreground-
/// only, so models cannot send fields those executors would ignore.
pub const fn managed_background_bash_supported() -> bool {
    cfg!(unix)
}

pub fn enable_managed_background_bash_schema(schemas: &mut [Value]) {
    // The Edge managed-service executor relies on Unix process/session
    // primitives. Do not advertise arguments which the Windows executor
    // rejects at runtime.
    if !managed_background_bash_supported() {
        return;
    }
    let Some(bash) = schemas
        .iter_mut()
        .find(|schema| schema.pointer("/function/name").and_then(Value::as_str) == Some("bash"))
    else {
        return;
    };
    if let Some(description) = bash.pointer_mut("/function/description") {
        *description = Value::String(
            "Files: use source_artifacts before spawn; checksum is not backup. Foreground has no persistence guarantee. Self-daemonizing services need run_in_background + ready_check."
                .to_string(),
        );
    }
    let Some(properties) = bash
        .pointer_mut("/function/parameters/properties")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    properties.insert(
        "run_in_background".to_string(),
        json!({"type":"boolean","default":false,"description":"Start an authorized environment-lifetime service and return after ready_check succeeds. Use for every process that must survive the call, including self-daemonizing programs. Do not append &, nohup, or setsid."}),
    );
    properties.insert(
        "ready_check".to_string(),
        json!({"type":"string","description":"Required with run_in_background=true. An independent side-effect-free command that proves readiness."}),
    );
    properties.insert(
        "background_ttl".to_string(),
        json!({"type":"number","minimum":1,"maximum":3600,"default":900,"description":"Maximum managed service lifetime in seconds."}),
    );
}

/// Check whether a tool name has a corresponding schema in the built-in
/// registry. Used by [`super::tool_engine::ToolEngine::register_handler`]
/// to detect schema↔handler mismatches at registration time rather than
/// at runtime when the LLM calls an unimplemented or mis-specified tool.
pub fn schema_exists_for_tool(name: &str) -> bool {
    all_tool_schemas().iter().any(|schema| {
        schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            == Some(name)
    })
}

/// Project every action-shaped schema onto the selected execution surface.
/// Action availability is declarative schema data, not a tool-name special
/// case, so future consolidated tools inherit the same visibility invariant.
pub fn project_action_schemas_for_surface(schemas: &mut [Value], surface: &str) {
    for schema in schemas {
        let surface_description = schema
            .pointer("/function/parameters")
            .and_then(Value::as_object)
            .and_then(|parameters| parameters.get(SURFACE_DESCRIPTIONS_KEY))
            .and_then(Value::as_object)
            .and_then(|descriptions| descriptions.get(surface))
            .and_then(Value::as_str)
            .map(str::to_string);
        let Some(parameters) = schema
            .pointer_mut("/function/parameters")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let Some(action_surfaces) = parameters
            .get(ACTION_SURFACES_KEY)
            .and_then(Value::as_object)
            .cloned()
        else {
            continue;
        };
        let allowed_actions = action_surfaces
            .iter()
            .filter(|(_, surfaces)| {
                surfaces.as_array().is_some_and(|surfaces| {
                    surfaces.iter().any(|item| item.as_str() == Some(surface))
                })
            })
            .map(|(action, _)| action.clone())
            .collect::<std::collections::HashSet<_>>();
        let allowed_properties = parameters
            .get(PER_ACTION_ALLOWED_KEY)
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|per_action| {
                per_action.iter().filter_map(|(action, properties)| {
                    allowed_actions
                        .contains(action)
                        .then_some(properties)
                        .and_then(Value::as_array)
                })
            })
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();

        if let Some(properties) = parameters
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        {
            if let Some(actions) = properties
                .get_mut("action")
                .and_then(Value::as_object_mut)
                .and_then(|action| action.get_mut("enum"))
                .and_then(Value::as_array_mut)
            {
                actions.retain(|action| {
                    action
                        .as_str()
                        .is_some_and(|action| allowed_actions.contains(action))
                });
            }
            if !allowed_properties.is_empty() {
                properties.retain(|name, _| allowed_properties.contains(name));
            }
        }
        for key in [
            PER_ACTION_REQUIRED_KEY,
            PER_ACTION_ANY_OF_REQUIRED_KEY,
            PER_ACTION_ALLOWED_KEY,
        ] {
            if let Some(map) = parameters.get_mut(key).and_then(Value::as_object_mut) {
                map.retain(|action, _| allowed_actions.contains(action));
            }
        }
        if let Some(summary) = parameters
            .get(SURFACE_DISCOVERY_SUMMARIES_KEY)
            .and_then(Value::as_object)
            .and_then(|summaries| summaries.get(surface))
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            parameters.insert(
                "x-astra-discovery-summary".to_string(),
                Value::String(summary),
            );
        }
        parameters.remove(ACTION_SURFACES_KEY);
        parameters.remove(SURFACE_DESCRIPTIONS_KEY);
        parameters.remove(SURFACE_DISCOVERY_SUMMARIES_KEY);
        if let Some(description) = surface_description {
            schema["function"]["description"] = Value::String(description);
        }
    }
}

/// Rebind stripped wire schemas to canonical action ownership before applying
/// a second execution-surface projection. Thin/local clients intentionally
/// remove internal ownership metadata from their provider schema; a server
/// receiving that schema must recover ownership from its trusted catalog,
/// never from client-authored declarations.
pub fn project_action_schemas_for_surface_using_declarations(
    schemas: &mut [Value],
    declarations: &[Value],
    surface: &str,
) {
    for schema in schemas.iter_mut() {
        let Some(name) = schema
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Some(declaration) = declarations.iter().find(|declaration| {
            declaration
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        }) else {
            continue;
        };
        if declaration
            .pointer("/function/parameters")
            .and_then(Value::as_object)
            .is_some_and(|parameters| parameters.contains_key(ACTION_SURFACES_KEY))
        {
            *schema = declaration.clone();
        }
    }
    project_action_schemas_for_surface(schemas, surface);
}

/// Replace the `run_script` schema with the narrowed server-side variant.
#[cfg(unix)]
pub fn narrow_run_script_for_server(schemas: &mut [Value]) {
    if let Some(slot) = schemas.iter_mut().find(|schema| {
        schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            == Some("run_script")
    }) {
        *slot = run_script_schema_for(SERVER_RUN_SCRIPT_RPC_TOOL_NAMES);
    }
}

/// Default `run_script` schema exposed when the caller has not yet supplied
/// a session-specific enabled-tool set. Uses the full RPC allowlist in
/// Project mode. Sites that know the session context should call
/// `run_script::build_run_script_schema` directly for a tighter schema.
#[cfg(unix)]
fn run_script_schema_default() -> Value {
    run_script_schema_for(crate::run_script::RPC_ALLOWED_TOOLS)
}

#[cfg(unix)]
fn run_script_schema_for(enabled_tool_names: &[&str]) -> Value {
    use std::collections::HashSet;
    let enabled: HashSet<String> = enabled_tool_names
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    crate::run_script::build_run_script_schema(&enabled, crate::run_script::ExecutionMode::Project)
}

// Keep schema construction incremental. A `vec![large_json!, ...]` first
// materializes the entire fixed-size element array on the caller's stack
// before moving it into the Vec. The complete built-in registry is large
// enough to overflow Tokio's default worker stack on a fresh process's first
// tool validation. Repetition into individual `push` statements keeps only
// one schema temporary live at a time while preserving order.
#[inline(never)]
fn push_built_in_schema(schemas: &mut Vec<Value>, build: impl FnOnce() -> Value) {
    schemas.push(build());
}

macro_rules! heap_schema_vec {
    ($($schema:expr),* $(,)?) => {{
        let mut schemas = Vec::new();
        $(push_built_in_schema(&mut schemas, || $schema);)*
        schemas
    }};
}

fn all_tool_schemas_core() -> Vec<Value> {
    heap_schema_vec![
        start_work_schema(),
        run_next_work_item_schema(),
        settle_work_item_schema(),
        inspect_work_plan_schema(),
        propose_work_plan_schema(),
        inspect_work_criteria_schema(),
        propose_work_criteria_schema(),
        json!({
            "type": "function",
            "function": {
                "name": "display_sixel",
                "description": "Render an image file (PNG, JPEG, GIF, etc.) inline in the terminal using sixel graphics. Requires img2sixel (libsixel) and a sixel-capable terminal. Use this after creating a plot or image in /tmp — first generate the image file, then call display_sixel to show it. In the interactive TUI the image is shown on a paused screen; press Enter to return. Raises an error if img2sixel is not installed or the file cannot be converted.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the image file to display (e.g. /tmp/sin_plot.png)."
                        }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
            "description": "source_artifacts preserves them before spawn. Checksum alone is not a backup. Foreground calls provide no process-persistence guarantee.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "mode": {"type": "string", "enum": ["verify"], "description": "Optional explicit workspace verification contract. Use only for a foreground verification command after edits. It succeeds only when the command exits zero and the executor proves the bound workspace stayed unchanged; do not use it for commands that write files."},
                    "timeout": {"type": "number", "default": crate::shell_ops::DEFAULT_BASH_TIMEOUT_SECS, "description": "Outer execution timeout in seconds. Set this field to a larger value for long builds/tests, e.g. cargo build or full test suites. A `timeout ...` program inside command does not extend Astra's outer timeout."},
                        "force": {"type": "boolean", "description": "Bypass the per-session identical-command cache."},
                        "source_artifacts": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": crate::source_preimage::MAX_SOURCE_ARTIFACTS,
                            "items": {"type": "string", "minLength": 1},
                            "description": "Optional hard evidence-preservation guarantee. List existing regular files relative to the workspace root before a command may open or transform irreplaceable inputs. Each file is copied and checksum-verified before the shell starts; any invalid path, capture failure, or race prevents execution. This is not a glob and a checksum alone is not a backup."
                        },
                        "external_state_paths": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 16,
                            "items": {"type": "string", "minLength": 1},
                            "description": "Optional executor-owned external-effect contract. For a requested change outside the bound workspace, list the smallest absolute external roots whose state must change. Astra captures bounded pre/post fingerprints and issues completion evidence only for an observed delta under authoritative process ownership. Paths inside or overlapping the workspace, relative/traversal paths, unobservable roots, background tasks, and unchanged state fail closed. Do not use this for workspace files."
                        }
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents. Fields: path,start_line,end_line,outline; ranges are inclusive 1-based; omit end_line to read through EOF; outline=true returns signatures. Complete source-read opaque markers may be copied unchanged to the corresponding editor; never recover hidden text.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "start_line": {"type": "integer", "minimum": 1, "description": "1-based first line of an inclusive range."},
                        "end_line": {"type": "integer", "minimum": 1, "description": "1-based final line of an inclusive range. Omit to read to end."},
                        "outline": {"type": "boolean", "description": "Return only function/class/struct signatures with line numbers"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "publish_artifact",
                "description": "Publish an existing workspace file as a durable session artifact for later preview or download. The file is copied into the authenticated session artifact store; this does not replace ordinary source edits or Work evidence. Paths must resolve under the bound workspace or /tmp, and files larger than 16 MiB are rejected.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "minLength": 1, "description": "Existing file path under the bound workspace or /tmp."},
                        "title": {"type": "string", "minLength": 1, "maxLength": 160, "description": "Optional display title; defaults to the filename."},
                        "description": {"type": "string", "minLength": 1, "maxLength": 1000, "description": "Optional short description shown with the artifact."},
                        "artifact_kind": {"type": "string", "minLength": 1, "maxLength": 64, "pattern": "^[A-Za-z0-9_.-]+$", "description": "Optional stable artifact category; inferred from the file when omitted."},
                        "content_type": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Optional MIME content type; inferred from the file when omitted."}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create, overwrite, or delete a file. For writes, provide `path` and `content`. Use this for new files, complete rewrites, or large changes (>4KB) — `str_replace` is a diff channel and should not be used for full-section replacements. WARNING: overwrites existing files silently — read first if you need to preserve content. For deletes, set `delete=true` and omit `content`. Retry `write_file` with corrected args; do not switch to bash or python just to write a file.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "content": {"type": "string", "description": "File content. Required unless deleting."},
                        "delete": {"type": "boolean", "description": "Delete instead of write. Omit content when true."}
                    },
                    "required": ["path"],
                    "x-astra-per-action-required": {
                        "write": ["path", "content"],
                        "delete": ["path"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "str_replace",
                "description": "Targeted replacement: single path+old_str+new_str or batch edits[]. Complete source-read opaque markers are safe old_str anchors; display-only/foreign/stale markers are invalid.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root. Required for single mode and same-file batch mode; optional when every edits[] entry has its own path."},
                        "old_str": {"type": "string", "description": "String to replace. Required with new_str in single-edit mode; omit when using edits."},
                        "new_str": {"type": "string", "description": "Replacement text. Required with old_str in single-edit mode; omit when using edits."},
                        "edits": {
                            "type": "array",
                            "description": "Batch mode: array of {old_str, new_str, path?} edits. Top-level path applies to entries without path. If top-level path is omitted, every edit must include path. Mutually exclusive with top-level old_str/new_str.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "path": {"type": "string", "description": "Optional file path for this edit; required when top-level path is omitted."},
                                    "old_str": {"type": "string"},
                                    "new_str": {"type": "string"}
                                },
                                "required": ["old_str", "new_str"]
                            }
                        },
                        "dry_run": {"type": "boolean", "description": "Preview without applying."},
                        "replace_all": {"type": "boolean", "description": "Replace all occurrences."},
                        "allow_structural_change": {"type": "boolean", "description": "Bypass structural safety checks for intentional syntax-breaking edits."}
                    },
                    "x-astra-per-action-required": {
                        "single": ["path", "old_str", "new_str"],
                        "batch_same_file": ["path", "edits"],
                        "batch_multi_file": ["edits[].path", "edits[].old_str", "edits[].new_str"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_file_edits",
                "description": "List or restore file edits recorded by write_file and str_replace. Use scope=current_turn to undo this turn's recorded file edits, scope=file with path to restore the latest recorded edit for one file, scope=turn with turn_index to restore a previous turn, scope=list to inspect file edit entries, or scope=source_receipt with receipt_id to restore an executor-retained source preimage.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list","source_receipt"], "description": "Rollback scope. Defaults to current_turn; path implies file scope."},
                        "path": {"type": "string", "description": "File path for scope=file."},
                        "receipt_id": {"type": "string", "description": "Opaque source preimage receipt ID for scope=source_receipt."},
                        "turn_index": {"type": "integer", "description": "Turn index for scope=turn."},
                        "file_after_sequence": {"type": "integer", "description": "Only restore file edits recorded after this journal sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List directory contents. Use to explore project structure or find files. For pattern-based file search (e.g. '**/*.rs'), use glob instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Directory path (default: project root)"},
                        "depth": {"type": "integer", "description": "Max depth (default 1)"}
                    },
                    "required": []
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents with a regex pattern. Respects .gitignore.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Regex pattern to search for"},
                        "path": {"type": "string", "description": "Directory or file to search."},
                        "include": {"type": "string", "description": "Optional file glob filter, e.g. '*.rs'."},
                        "case_sensitive": {"type": "boolean", "description": "Case-sensitive search."},
                        "fixed_strings": {"type": "boolean", "description": "Treat pattern as a literal string."},
                        "max_matches": {"type": "integer", "description": "Max matches per file"},
                        "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "content, files_with_matches, or count."}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "glob",
                "description": "Find files matching a glob pattern. Supports pagination via offset/head_limit and sorting by mtime or path. Use for pattern-based file search (e.g. '**/*.rs', 'src/**/test_*'); use list_dir for interactive directory exploration instead.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "Glob pattern e.g. '**/*.rs'"},
                        "path": {"type": "string", "description": "Root directory."},
                        "sort_by": {"type": "string", "enum": ["mtime", "path"], "description": "Sort by newest mtime or by path."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Skip first N matching files (for pagination)"},
                        "head_limit": {"type": "integer", "minimum": 0, "description": "Max files after offset. Default 100; 0 = unlimited."}
                    },
                    "required": ["pattern"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "symbols",
                "description": "Extract code symbols (functions, classes, structs, methods) from a file using AST parsing (tree-sitter). Supports Rust, Python, TypeScript/JavaScript, Go, Java, C/C++, Ruby. Returns structured symbol info with signatures, line numbers, and nesting. Set calls=true to show function calls within each symbol body (understand code flow without reading full source). Use kinds[] to filter by symbol type (fn, method, class, struct, trait, etc.), and pattern for regex name filtering. Use for: understanding file structure, finding specific symbols, generating documentation outlines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "File path relative to project root"},
                        "pattern": {"type": "string", "description": "Optional regex pattern to filter symbols by name (e.g., 'test_', 'parse.*')"},
                        "kinds": {"type": "array", "items": {"type": "string"}, "description": "Optional filter by symbol kinds: fn, method, class, struct, trait, interface, enum, type, const, var"},
                        "calls": {"type": "boolean", "description": "If true, show function calls within each symbol's body. Helps understand code flow without reading full source."}
                    },
                    "required": ["path"]
                }
            }
        }),
        // ── Git mutation tools ─────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL and return structured JSON with metadata, extracted content (Markdown by default), and navigation links. Handles HTML-to-Markdown conversion, link discovery, and content truncation automatically.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "URL to fetch (http:// or https://)"},
                        "format": {"type": "string", "enum": ["markdown", "text"], "description": "Output format for extracted content (default: markdown)"},
                        "max_content": {"type": "integer", "description": "Max extracted content characters (default 24576; increase when the full page is needed)"},
                        "timeout": {"type": "integer", "description": "Timeout in seconds (default 30)"},
                        "max_links": {"type": "integer", "description": "Max navigation links to extract (default 25)"}
                    },
                    "required": ["url"]
                }
            }
        }),
        // ─── Web search tool ──────────────────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Perform a web search and return the fetched result page as structured JSON with extracted Markdown and result links. Use for current information, documentation, or answers not in local knowledge; do not call web_fetch on the search page separately.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query. Be specific for better results."
                        },
                        "engine": {
                            "type": "string",
                            "enum": ["google", "duckduckgo", "bing", "wikipedia", "github"],
                            "description": "Search engine to use (default: bing). Use 'wikipedia' for encyclopedic info, 'github' for code/repos."
                        },
                        "num_results": {
                            "type": "integer",
                            "description": "Number of results to request (default: 10, max: 50)",
                            "default": 10
                        }
                    },
                    "required": ["query"]
                }
            }
        }),
        // ── Language Server Protocol ───────────────────────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "lsp",
                "description": "Language Server Protocol operations. Set dry_run=false to apply writes (rename, format, code_action). WARNING: dry_run=false is a third write path alongside write_file and str_replace — it modifies files in-place via the LSP. Default true (preview-only).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "operation": {
                            "type": "string",
                            "enum": [
                                "goto_definition","find_references","hover","document_symbols",
                                "workspace_symbols","call_hierarchy","incoming_calls","outgoing_calls",
                                "declaration","type_definition","implementation","supertypes","subtypes",
                                "prepare_rename","rename","code_actions","completions","signature_help",
                                "document_highlight","document_links","inlay_hints","folding_ranges",
                                "document_colors","color_presentations","semantic_tokens","code_lenses",
                                "selection_ranges","linked_editing_range",
                                "format_document","format_range","format_on_type","diagnostics"
                            ]
                        },
                        "file": {"type": "string", "description": "File path"},
                        "line": {"type": "integer", "description": "1-based line number"},
                        "column": {"type": "integer", "description": "1-based column"},
                        "end_line": {"type": "integer", "description": "End line (range ops)"},
                        "end_column": {"type": "integer", "description": "End column (range ops)"},
                        "symbol": {"type": "string", "description": "Symbol name (alternative to line/column)"},
                        "query": {"type": "string", "description": "Query (workspace_symbols)"},
                        "new_name": {"type": "string", "description": "New name (rename)"},
                        "dry_run": {"type": "boolean", "description": "Preview mode (default true)"},
                        "action_index": {"type": "integer", "minimum": 0, "description": "Code action index (default 0)"},
                        "item_index": {"type": "integer", "minimum": 0, "description": "Item index (completions/code_lenses)"},
                        "scope": {"type": "string", "enum": ["file", "project"]}
                    },
                    "required": ["operation"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "git",
                "description": "Git operations: status, diff, log, show, blame, commit, stash, push, and worktree. Pass action as the first parameter.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": crate::git_tool_contract::GIT_ACTIONS,
                            "description": "Git operation to perform"
                        },
                        "path": {
                            "type": "string",
                            "description": "Repository-relative file or directory path. Used by: diff (one filter only), log, blame, checkout_file, contributors. For a diff over more than one path, use `paths`; never concatenate multiple paths into this string."
                        },
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 1,
                            "description": "Canonical multi-path filter for git(action=diff) only. Pass one repository-relative path per array item (for example [\"src/a.rs\", \"src/b.rs\"]); mutually exclusive with `path`."
                        },
                        "file": {
                            "type": "string",
                            "description": "Repository-relative file path. Used by: file_history (required)."
                        },
                        "start_line": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 4294967295_u64,
                            "description": "1-based first line for git(action=blame). Omit both line bounds to blame the whole file."
                        },
                        "end_line": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 4294967295_u64,
                            "description": "1-based inclusive final line for git(action=blame). Defaults to start_line when only start_line is supplied."
                        },
                        "ref": {
                            "type": "string",
                            "description": "Git ref — commit SHA, branch, or tag. Used by: diff (compares ref vs worktree), log (restrict to ref), checkout_file (required: ref to restore from). Defaults to HEAD when omitted."
                        },
                        "base_ref": {
                            "type": "string",
                            "description": "Single base ref for range diffs. Used by diff with ref as base_ref..ref. For a complete A..B or A...B range, pass it in ref and omit base_ref."
                        },
                        "revision": {
                            "type": "string",
                            "description": "Commit-ish to inspect. Used by: show. Defaults to HEAD."
                        },
                        "staged": {
                            "type": "boolean",
                            "description": "Show staged (index vs HEAD) changes. Used by: diff. Default false."
                        },
                        "stat_only": {
                            "type": "boolean",
                            "description": "Return only file/change statistics instead of patch content. Used by: diff and show. Default false."
                        },
                        "n": {
                            "type": "integer",
                            "description": "Max entries to return. Used by: log (default 10, max 500 auto-throttled), file_history (default 10), log_search (default 200)."
                        },
                        "query": {
                            "type": "string",
                            "description": "Commit-message search query. Used by: log_search (required)."
                        },
                        "since": {
                            "type": "string",
                            "description": "Git date expression (e.g. '2.weeks.ago', '2024-01-01'). Used by: contributors."
                        },
                        "message": {
                            "type": "string",
                            "description": "Commit message. Used by: commit (required), stash (optional, with sub_action=push/save)."
                        },
                        "all": {
                            "type": "boolean",
                            "description": "Stage all tracked modifications before committing. Used by: commit. Default false."
                        },
                        "commit_sha": {
                            "type": "string",
                            "description": "Commit SHA to revert. Used by: revert_commit (required)."
                        },
                        "sub_action": {
                            "type": "string",
                            "description": "Sub-operation for multi-mode actions. Used by: stash (push/save/pop/apply/drop/list), worktree (add/list/remove)."
                        },
                        "index": {
                            "type": "integer",
                            "description": "Stash index (stash@{N}). Used by: stash with sub_action=apply/pop/drop. Default 0."
                        },
                        "stash_ref": {
                            "type": "string",
                            "description": "Exact stash selector or OID. Used by: stash with sub_action=apply. Takes precedence over index."
                        },
                        "remote": {
                            "type": "string",
                            "description": "Remote name (e.g. 'origin'). Used by: push (required)."
                        },
                        "branch": {
                            "type": "string",
                            "description": "Target branch name. Used by: push (required)."
                        },
                        "force_with_lease": {
                            "type": "boolean",
                            "description": "Use --force-with-lease (safer than bare --force). Used by: push. Default false."
                        },
                        "set_upstream": {
                            "type": "boolean",
                            "description": "Set upstream tracking (-u). Used by: push. Default false."
                        }
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "blame": ["path"],
                        "commit": ["message"],
                        "revert_commit": ["commit_sha"],
                        "file_history": ["file"],
                        "log_search": ["query"],
                        "stash": ["sub_action"],
                        "checkout_file": ["path", "ref"],
                        "worktree": ["sub_action"],
                        "push": ["remote", "branch"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "github",
                "description": "GitHub operations. Per-action required fields: get_pr/ci_status→pr_number, get_issue→issue_number, create_issue→title. `repo` (owner/name or bare name) defaults to the first preferred repo or is inferred from git remote; pass explicitly when querying cross-repo or a repo not in the preferred list.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list_prs","get_pr","ci_status","repo_stats","list_issues","get_issue","create_issue"], "description": "GitHub operation"},
                        "repo": {"type": "string", "description": "owner/name or bare name (e.g. 'anthropics/reference-agent' or 'memoria'). Inferred from current git remote when omitted."},
                        "pr_number": {"type": "integer", "description": "PR number. REQUIRED when action=get_pr or action=ci_status."},
                        "issue_number": {"type": "integer", "description": "Issue number. REQUIRED when action=get_issue."},
                        "title": {"type": "string", "description": "Issue title. REQUIRED when action=create_issue."},
                        "body": {"type": "string", "description": "Issue body (create_issue)."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "get_pr": ["pr_number"],
                        "ci_status": ["pr_number"],
                        "get_issue": ["issue_number"],
                        "create_issue": ["title"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "memory",
                "description": "Memory evidence. Recall is advisory. Reuse exact memory_id or selection_id; never invent IDs.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["remember","recall","session_audit","expand","forget","update","reflect","profile","feedback"],
                            "description": "Operation. session_audit reports extraction lifecycle, not stored records; recall is ranked, not a count."
                        },
                        "content": {"type": "string"},
                        "query": {"type": "string"},
                        "memory_id": {"type": "string", "description": "Exact opaque ID from evidence; never invent."},
                        "memory_ids": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 64,
                            "items": {"type": "string"},
                            "description": "Exact IDs from one surfaced selection; max 64."
                        },
                        "selection_id": {
                            "type": "string",
                            "description": "Session-scoped receipt for referential follow-ups; never invent."
                        },
                        "memory_type": {
                            "type": "string",
                            "enum": ["semantic","profile","procedural","working","episodic"],
                            "description": "Category."
                        },
                        "top_k": {"type": "integer"},
                        "min_confidence": {"type": "number"},
                        "scope": {
                            "type": "string",
                            "enum": ["all","session"]
                        },
                        "view": {
                            "type": "string",
                            "enum": ["compact","overview","full"]
                        },
                        "importance": {
                            "type": "number",
                            "minimum": 0.0,
                            "maximum": 1.0,
                            "description": "Optional numeric salience from 0.0 (low) to 1.0 (high); do not use labels such as low/high."
                        },
                        "trust_tier": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "tags_add": {"type": "array", "items": {"type": "string"}},
                        "tags_remove": {"type": "array", "items": {"type": "string"}},
                        "visibility": {
                            "type": "string",
                            "enum": ["private","team"]
                        },
                        "team_id": {
                            "type": "string",
                            "description": "Team id for team visibility."
                        },
                        "reason": {"type": "string", "description": "Required audit reason for correction or intentional deletion."},
                        "level": {
                            "type": "string",
                            "enum": ["abstract","overview","detail","linked"],
                            "description": "expand depth."
                        },
                        "signal": {
                            "type": "string",
                            "enum": ["useful","irrelevant","outdated","wrong"],
                            "description": "Attributed quality evidence: outdated means once-valid but stale; wrong means false."
                        },
                        "context": {"type": "string"},
                        "agent_type": {
                            "type": "string",
                            "enum": ["explore","code-review","task","general-purpose"]
                        }
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "remember": ["content"],
                        "recall": ["query"],
                        "expand": ["memory_id"],
                        "forget": ["reason"],
                        "update": ["reason"],
                        "feedback": ["memory_id", "signal"]
                    },
                    "x-astra-per-action-any-of-required": {
                        "forget": [["memory_id"], ["memory_ids"], ["selection_id"]],
                        "update": [["memory_id"], ["query"]]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "session",
                "description": "Session lifecycle and history. Actions: config(path+value), sleep, history_page, history_search, history_around. Use dedicated tools, when visible in the current tool surface, for file rollback (`rollback_file_edits`), session-state rollback (`rollback_session_state`), context compression (`compress_context`), plan lifecycle (`enter_plan_mode`/`exit_plan_mode`), and user questions (`ask_user`).",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "action": {"type": "string", "enum": ["config","sleep","history_page","history_search","history_around"]},
                        "path": {"type": "string", "description": "Config path for action=config."},
                        "value": {"type": "string", "description": "Config value"},
                        "force": {"type": "boolean", "description": "Override config drift/mutation governor for action=config."},
                        "duration_ms": {"type": "integer", "description": "Sleep ms, max 300000"},
                        "reason": {"type": "string", "description": "Reason (sleep)"},
                        "pattern": {"type": "string", "description": "history_search search text: compact topic, phrase, filename, error text, decision, or Chinese/English keyword."},
                        "before_seq": {"type": "integer", "description": "history_page/history_search cursor: return transcript rows older than this item_seq."},
                        "after_seq": {"type": "integer", "description": "history_page/history_search cursor: return transcript rows newer than this item_seq."},
                        "item_seq": {"type": "integer", "description": "history_around anchor returned by history_page/history_search."},
                        "radius": {"type": "integer", "description": "history_around rows before and after item_seq, 0-10, default 3."},
                        "limit": {"type": "integer", "description": "history_page/history_search row/result limit. history_page: 1-50 default 20; history_search: 1-20 default 8."},
                        "scan_limit": {"type": "integer", "description": "history_search recent transcript scan limit, 50-1000, default 400."},
                        "order": {"type": "string", "enum": ["asc","desc"], "description": "history_page output order. asc reads a recovered range chronologically; desc browses backward from newest."},
                        "role": {"type": "string", "enum": ["all","user","assistant","system"], "description": "Optional history role filter. Default all."}
                    },
                    "required": ["action"],
                    "x-astra-per-action-required": {
                        "config": ["path", "value"],
                        "history_search": ["pattern"],
                        "history_around": ["item_seq"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "compress_context",
                "description": "Record a manual context-compression request for the current turn. Use when the session is carrying stale or bulky context and future turns should prefer a compacted history.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "reason": {"type": "string", "description": "Short reason for manual compression. Defaults to manual_request."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_session_state",
                "description": "List or restore server-side session-state mutations such as config overrides, task-state snapshots, and manual context-compression markers. This is for session state, not file contents; use rollback_file_edits for file rollback.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn", "turn", "list"], "description": "Rollback scope. Defaults to current_turn. Use list to inspect available rollback handles."},
                        "turn_index": {"type": "integer", "description": "Turn index when scope=turn."},
                        "session_state_after_sequence": {"type": "integer", "description": "Only restore entries recorded after this rollback-journal sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "mo_query",
                "description": "Run a MatrixOne SQL query. Destructive statements are blocked unless allow_destructive=true, and mutating queries capture a pre-state snapshot for rollback_database_snapshots.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "sql": {"type": "string", "description": "SQL to execute."},
                        "database": {"type": "string", "description": "Optional MatrixOne database name."},
                        "allow_destructive": {"type": "boolean", "description": "Explicitly allow destructive or mutating SQL when needed. Default false."}
                    },
                    "required": ["sql"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "rollback_database_snapshots",
                "description": "List or restore MatrixOne pre-state snapshots captured before mutating SQL. Use this for database rollback, not file or session-state rollback.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn", "turn", "snapshot", "list"], "description": "Rollback scope. Defaults to current_turn. Use list to inspect recorded snapshots."},
                        "turn_index": {"type": "integer", "description": "Turn index when scope=turn."},
                        "snapshot_id": {"type": "string", "description": "Snapshot identifier when scope=snapshot."},
                        "database": {"type": "string", "description": "Optional database name when restoring a specific snapshot."},
                        "database_after_sequence": {"type": "integer", "description": "Only restore database snapshot entries recorded after this journal sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Actions: spawn needs description+prompt (not task/type/agent_id; foreground fan-in by default; no background arg); get_result needs the returned agent_id of explicitly backgrounded work; run_chain needs name+description+steps.\n\n\
         Multi-agent and local fixed-chain operations. Actions: spawn, get_result, run_chain, send_message. `run_chain` is a local executor pipeline, not a durable task list. If the user asks for task/Work tracking and `start_work` is visible, call `start_work` directly instead of using `agent`.\n\n\
         ## Required fields per action\n\
         - `spawn`: REQUIRES `action`, `description`, `prompt`. (Optional: `agent_type`, `model`, `max_turns`, `max_output_tokens`, `complexity`, `isolated`, `allowed_tools`, `name`, `inherit_prefix`.)\n\
         - `get_result`: REQUIRES `action`, `agent_id`.\n\
         - `run_chain`: REQUIRES `action`, `name`, `description`, `steps`.\n\
         - `send_message`: REQUIRES `action`, `to`, `message`; returns `queued`, then the receiver emits an applied acknowledgement at its next model boundary.\n\n\
         For `spawn`, pass both non-empty fields: `description` (short UI summary) and `prompt` (full child brief). Do NOT pass a top-level `task` field. Do NOT pass `type`; use `agent_type`. Do NOT pass `inherit_context`. `agent_id` is ONLY for `get_result`; never prefill it on `spawn`. Astra generates that runtime id for you. Later `get_result` calls must reuse the exact returned `agent_id`. If you need a mailbox label, use `name`, but `name` is not valid for `get_result`.\n\n\
         ## Spawn example\n\
         `{\"action\":\"spawn\",\"description\":\"Audit auth flow\",\"prompt\":\"Read src/auth/* and report token-handling bugs. Return numbered findings.\",\"agent_type\":\"general-purpose\"}`\n\n\
         ## Execution mode\n\
         `spawn` is foreground by contract: it waits for the child's terminal result while the runtime streams live progress and keeps client controls responsive. In the terminal, the user may explicitly press Ctrl+B to move the live child to the background; do not pass a background flag. A background handoff returns a stable agent_id/run_id and its terminal result is delivered to the parent mailbox.\n\n\
         ## Parallel sub-agent fan-out\n\
         For a fixed-size parallel group, call `agent_fanout` with its JSON object schema; do not simulate it with an `agents:[...]` payload on `agent`. Slots may include `id` as a caller-facing label; runtime-generated `agent_id` values come back in the result.\n\
         For plan lifecycle, if `enter_plan_mode` / `exit_plan_mode` are visible in the current tool surface, call them directly; never wrap them in the `agent` `run_chain` action.\n\
         Do NOT pass an `agents:[...]` payload, do NOT pass a top-level `task` field, and do NOT wrap spawn arguments under a `spawn` field. `agent` launches one child; `agent_fanout` launches a fixed parallel group.

         ## Canonical Work and delegation
         - `agent(spawn)` + optional `agent(get_result)`: one foreground sub-agent, or one explicitly backgrounded child the user later inspects.
         - `agent_fanout`: fixed-size parallel sub-agent groups with target-count accounting.
         - Shell commands/processes are separate execution tools; do not represent them as sub-agents.
         - When no canonical Work exists and the current turn requires durable task tracking, establish it with `start_work` before delegating. When canonical Work already exists, keep that Work as the durable scope rather than trying to create another one. `agent` and `agent_fanout` do not themselves create or replace a canonical task list.
         - `start_work` may return `initial_task`, and `settle_work_item` may return `next_task`. Each is already the server-selected primary-session assignment: execute it directly. Call `run_next_work_item({})` only when neither response supplied an assignment. Treat an assigned task's expected result as its stop boundary: gather sufficient direct evidence, settle immediately when satisfied, and do not expand into adjacent investigation. Generic `agent` and `agent_fanout` are reserved for real isolation or parallelism boundaries; a WorkItem alone is not a delegation reason.
         - Background task tools only observe or control execution; they are not a planning system.",
                "parameters": {
                    "type": "object",
                    "x-astra-action-surfaces": {
                        "spawn": ["local", "server"],
                        "get_result": ["local", "server"],
                        "run_chain": ["local"],
                        "send_message": ["local", "server"]
                    },
                    "x-astra-surface-descriptions": {
                        "server": "Server-owned single-agent lifecycle. Actions: spawn, get_result, send_message. This tool does not create a durable task list: when the user asks for task/Work tracking, call the visible start_work tool directly. For a fixed-size parallel group use agent_fanout."
                    },
                    "x-astra-surface-discovery-summaries": {
                        "server": "spawn: action+description+prompt; foreground fan-in unless the user backgrounds it. get_result: action+agent_id. send_message: action+to+message. Durable task lists use start_work."
                    },
                    "x-astra-discovery-summary": "spawn: action+description+prompt; foreground fan-in unless the user backgrounds it. get_result: action+agent_id. run_chain: local fixed pipeline with action+name+description+steps, never a durable task list. send_message: action+to+message. Durable task lists use the separate start_work tool.",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","get_result","run_chain","send_message"]},
                        "steps": {
                            "type": "array",
                            "minItems": 1,
                            "description": "run_chain steps.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "tool": {"type": "string"},
                                    "args": {"type": "object"},
                                    "output_key": {"type": "string"},
                                    "skip_if_prev_contains": {"type": "string"}
                                },
                                "required": ["tool", "args"]
                            }
                        },
                        "description": {"type": "string", "description": "Short operation description when required by the selected action."},
                        "prompt": {"type": "string", "description": "Full child task brief for spawn. Non-empty and required with description."},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Sub-agent persona (spawn). Default: general-purpose."},
                        "model": {"type": "string", "description": "Model override (spawn). Default: parent's model."},
                        "name": {"type": "string", "description": "Action label when accepted by the selected action."},
                        "input": {"type": "object", "description": "Optional run_chain template input."},
                        "rollback_on_failure": {"type": "boolean", "description": "Rollback bounded chain mutations after failure."},
                        "max_turns": {"type": "integer", "minimum": 1, "description": "Numeric child ceiling. When complexity is also present, the smaller of the numeric and complexity-derived ceilings wins."},
                        "max_output_tokens": {"type": "integer", "minimum": 1, "description": "Optional first child request output-token ceiling."},
                        "inherit_prefix": {
                            "type": ["object", "null"],
                            "description": "Optional exact parent prefix-cache inheritance request. Omit for a fresh child prefix; set required=true only when fallback is unacceptable.",
                            "properties": {
                                "from_run_id": {"type": ["string", "null"]},
                                "required": {"type": "boolean"}
                            },
                            "additionalProperties": false
                        },
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity ceiling: `light`≤10 turns, `normal`=agent default, `deep`=2× default. Prefer normal for scoped review/refactor work; use deep only when this child independently needs broad multi-step investigation. It never expands a smaller max_turns."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (spawn)"},
                        "work_item": {
                            "type": "object",
                            "description": "Optional exact canonical WorkItem revision assigned to this child. Use an item returned by start_work or inspect_work_plan; the server verifies current Work membership and derives the attempt from the child run.",
                            "properties": {
                                "item_id": {"type": "string", "minLength": 1},
                                "item_revision": {"type": "integer", "minimum": 1}
                            },
                            "required": ["item_id", "item_revision"],
                            "additionalProperties": false
                        },
                        "agent_id": {"type": "string", "description": "ONLY for action='get_result'. Must be the exact runtime-generated agent_id returned by a prior spawn, not the optional spawn name. Never prefill this on spawn."},
                        "to": {"type": "string", "description": "REQUIRED for action='send_message'. Active child/peer agent_id, related exact run_id within the current delegation boundary, 'parent', or '*' for broadcast."},
                        "message": {"description": "REQUIRED for action='send_message'. Message content."},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"]},
                        "request_id": {"type": "string", "description": "Optional correlation id when answering or following up on an earlier message."}
                    },
                    "required": ["action"],
                    "additionalProperties": false,
                    "x-astra-per-action-required": {
                        "spawn": ["description", "prompt"],
                        "run_chain": ["name", "description", "steps"],
                        "get_result": ["agent_id"],
                        "send_message": ["to", "message"]
                    },
                    "x-astra-per-action-allowed": {
                        "spawn": ["action", "description", "prompt", "agent_type", "model", "name", "max_turns", "max_output_tokens", "complexity", "isolated", "allowed_tools", "inherit_prefix", "work_item"],
                        "get_result": ["action", "agent_id"],
                        "run_chain": ["action", "name", "description", "steps", "input", "rollback_on_failure"],
                        "send_message": ["action", "to", "message", "message_type", "request_id"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent_fanout",
                "description": "Launch one atomic parallel agent group: start requires exactly target_count slots, each with description+prompt, and no brief/agents/background fields. Submit one complete JSON object; do not emit a DSL or a partial object.\n\n\
         Actions:\n\
         - `start`: requires `action`, `target_count`, and exactly target_count slots. Every slot has description+prompt; optional `id` is only a caller-facing label. Minimal valid start: `{\"action\":\"start\",\"target_count\":2,\"slots\":[{\"id\":\"api\",\"description\":\"Review API\",\"prompt\":\"Review the API and report findings.\"},{\"id\":\"ui\",\"description\":\"Review UI\",\"prompt\":\"Review the UI and report findings.\"}]}`. Shared optional configuration belongs in `defaults`; omit it unless needed.\n\
         - `get_results`: requires `action` and returned `group_id` for an explicitly backgrounded group. It takes a short non-blocking snapshot; terminal updates also arrive through the parent mailbox, so do not busy-poll. Use optional `slot_index`, `offset`, and `max_bytes` for one bounded result window; `results[].next_call` gives the next window.\n\
         - `stop_slot`: requires `action`, `group_id`, and `slot_index`; it stops one running child.\n\n\
         - `stop_group`: requires `action` and `group_id`; it requests cancellation for every non-terminal child in one group operation.\n\n\
         Use this for independent parallel work only when the user request or loaded workflow contains an explicit topology directive. Quality, scope, and complexity requirements alone do not imply extra agents or parallel execution; when neither authority explicitly requires delegation or parallelism, keep the work in the parent turn. Put each concise child instruction only in `slots[i].prompt`. Children inherit the current execution binding; only tools exposed in a child's own tool surface are usable. If `agent_type` is omitted, the server uses the bounded read-only `explore` persona; request `task` or `general-purpose` explicitly for mutation or full-surface work. Do not start workspace-dependent slots when the current workspace provider is unavailable. Never paste file contents, diffs, or prior tool output into a slot prompt. Fanout already decomposes work: keep each slot narrowly scoped and normally use `normal`; omit `max_turns` unless the user supplied a bound or the slot is small enough to reserve its final model boundary for synthesis. Do not mark every review slot `deep`. A per-slot or shared tool allowlist is named `allowed_tools`; there is no `tools` field. Use no brief/agents/background fields: never send top-level `brief`, `agents`, or `run_in_background`, and never put generated `agent_id` inside a slot. Start waits for accepted children concurrently and returns one canonical group result. In the terminal only the user may press Ctrl+B to hand the live group to the background; that explicit handoff returns stable child identities and later terminal results remain available through the group mailbox/get_results contract.",
                "parameters": {
                    "type": "object",
                     "x-astra-discovery-summary": "start: target_count + exactly that many slots; description+prompt each; no brief/agents/background; never embed diffs. Omit agent_type=read-only explore; task/general-purpose=mutation. Child surface authoritative.",
                    "properties": {
                        "action": {"type": "string", "enum": ["start","get_results","stop_slot","stop_group"]},
                        "group_id": {"type": "string", "description": "Fanout group id. Optional on start; required for get_results, stop_slot, and stop_group."},
                        "title": {"type": "string", "description": "Optional short label for the group."},
                        "target_count": {"type": "integer", "minimum": 1, "description": "REQUIRED for start. Fixed number of slots to launch; must equal slots.length."},
                        "slots": {
                            "type": "array",
                            "description": "REQUIRED for start. One entry per parallel child.",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "id": {"type": "string", "description": "Optional stable caller-facing label for this slot. Returned in start/results/fanout projections. Not the runtime agent_id."},
                                    "description": {"type": "string", "maxLength": crate::agent_tool_contract::AGENT_FANOUT_SLOT_DESCRIPTION_MAX_CHARS, "description": "Short UI summary for this slot."},
                                    "prompt": {"type": "string", "maxLength": crate::agent_tool_contract::AGENT_FANOUT_SLOT_PROMPT_MAX_CHARS, "description": "Concise child task brief. The child inherits current provider bindings and can use only its exposed tools; never paste file contents, diffs, or prior tool output here."},
                                    "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Child persona. Omit for bounded read-only explore; choose task/general-purpose explicitly for mutation or full-surface work."},
                                    "model": {"type": "string"},
                                    "max_turns": {"type": "integer", "minimum": 1},
                                    "max_output_tokens": {"type": "integer"},
                                    "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                    "isolated": {"type": "boolean"},
                                    "allowed_tools": {"type": "array", "items": {"type": "string"}}
                                },
                                "required": ["description", "prompt"]
                            }
                        },
                        "defaults": {
                            "type": "object",
                            "description": "Shared runtime configuration inherited by every slot. Slot-level overrides take precedence.",
                            "additionalProperties": false,
                            "properties": {
                                "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Shared child persona. Omit for bounded read-only explore; choose task/general-purpose explicitly for mutation or full-surface work."},
                                "model": {"type": "string"},
                                "max_turns": {"type": "integer", "minimum": 1},
                                "max_output_tokens": {"type": "integer"},
                                "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                "isolated": {"type": "boolean"},
                                "allowed_tools": {"type": "array", "items": {"type": "string"}}
                            }
                        },
                        "slot_index": {"type": "integer", "minimum": 0, "description": "REQUIRED for stop_slot. Optional for get_results to read one slot result window."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Optional for get_results with slot_index. Byte offset for the slot result window. Default 0."},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": 65536, "description": "Optional for get_results. Maximum result bytes per slot window. Default 8192, max 65536."}
                    },
                    "required": ["action"],
                    "additionalProperties": false,
                    "x-astra-per-action-required": {
                        "start": ["target_count", "slots"],
                        "get_results": ["group_id"],
                        "stop_slot": ["group_id", "slot_index"],
                        "stop_group": ["group_id"]
                    },
                    "x-astra-per-action-allowed": {
                        "start": ["action", "group_id", "title", "target_count", "slots", "defaults"],
                        "get_results": ["action", "group_id", "slot_index", "offset", "max_bytes"],
                        "stop_slot": ["action", "group_id", "slot_index"],
                        "stop_group": ["action", "group_id"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Read the live observation snapshot for the running turn/session. Call this before answering a user request to audit or reflect on runtime/session state, recent tool use, traces, or agent execution behavior; conversation history and session memory are not substitutes for runtime telemetry. Use for self-checks: token/cache pressure, step latency/performance, tool health, recent rounds, runtime errors, stall/noise state, working memory, and plan/task/session lifecycle/resume state including the last lifecycle event when available. Use artifact plus offset to read a bounded window from a persisted tool-result handle. CLI/Edge can also inspect local cache and session_memory artifacts. Introspect is live-only: a historical turn/session/cross-session horizon returns a clearly labeled recent live projection; use reflect for persisted causal evidence.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "topic": {"type": "string", "enum": ["overview","runtime","execution","knowledge"], "description": "Top-level observation area. Defaults to runtime; use execution for errors/trace and knowledge for session_memory/context artifacts."},
                        "facet": {"type": "string", "enum": ["session","overview","recent","errors","trace","volatile","stall","noise","cache","session_memory"], "description": "Specific live view. overview is the composite view and should be the single first call for a runtime retrospective: it includes session state, recent rounds/trace timing and tools, stall/noise, and errors. Do not fan out separate facets unless overview reports a concrete gap. cache and session_memory require CLI/Edge-local artifacts; unavailable providers are reported in data_coverage."},
                        "depth": {"type": "string", "enum": ["hint","summary","diagnostic","forensic"], "description": "Output depth. hint is a compact nudge; diagnostic/forensic use the bounded full live renderer, including step latency/performance when available. Use diagnostic with facet=overview for one-call retrospective evidence."},
                        "horizon": {"type": "string", "enum": ["now","current_turn","recent","turn","session","cross_session"], "description": "Observation window. now/current_turn/recent return live evidence directly. A historical turn/session/cross_session request returns a labeled recent live projection rather than failing; pair it with reflect for persisted evidence. Choose trace-like content with facet=trace, not by changing horizon."},
                        "question": {"type": "string", "description": "Optional caller context label. It does not widen the live evidence horizon or replace reflect for persisted causal analysis."},
                        "source_policy": {"type": "string", "enum": ["auto","live_only","live_first","durable_first","local_only","cloud_only"], "description": "Preferred data source. Missing or unsatisfied providers are reported instead of fabricated."},
                        "include_context": {"type": "boolean", "description": "Request visible prompt/context facts when a provider is available; these are observed context, not durable truth."},
                        "format": {"type": "string", "enum": ["text","json"], "description": "Output format. text is default; json returns a structured read-only observation envelope."},
                        "artifact": {"type": "string", "description": "An opaque session-scoped artifact://session/tool-result/<token> handle returned for an oversized tool result. When set, reads that result instead of a runtime snapshot."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Byte offset for artifact recovery. Start at 0 and continue with the returned next_offset."},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": 65536, "description": "Maximum bytes in one artifact window. Defaults to 8192; use returned next_offset to continue."}
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "reflect",
                "description": "Analyze persisted observation evidence for the active session. Use for causal questions across prior turns after errors, confusing tool choices, performance regressions, or trace review. For a user-requested runtime/session retrospective, make one composite topic=overview facet=overview call with the concrete question; this combines decisions, tools, errors, trace and provider coverage, so do not fan out facets unless it reports a gap. Pair this persisted view with introspect's live snapshot and label the evidence sources separately. Data may lag the current live turn; use introspect for immediate runtime health. Without an active session this returns reflect_requires_session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "topic": {
                            "type": "string",
                            "enum": ["overview", "runtime", "execution", "knowledge"],
                            "description": "Top-level persisted evidence area. overview is the composite default for retrospectives; use execution for a concrete errors/tools/trace gap, runtime for performance, knowledge for context/memory."
                        },
                        "facet": {
                            "type": "string",
                            "enum": ["overview", "performance", "errors", "tools", "trace", "context", "memory"],
                            "description": "Persisted evidence view under the selected topic. overview is the composite first call and includes decisions, tools, errors, trace and provider coverage. Do not fan out separate facets unless overview reports a concrete gap. Examples for targeted follow-up: topic=execution facet=errors, topic=execution facet=trace, topic=runtime facet=performance."
                        },
                        "depth": {
                            "type": "string",
                            "enum": ["hint", "summary", "diagnostic", "forensic"],
                            "description": "Analysis depth. forensic requests are still bounded by last_n and provider limits."
                        },
                        "horizon": {
                            "type": "string",
                            "enum": ["now", "current_turn", "recent", "turn", "session", "cross_session"],
                            "description": "Time range label. Persisted evidence is strongest for recent/session; trace is selected with facet=trace."
                        },
                        "source_policy": {
                            "type": "string",
                            "enum": ["auto", "live_only", "live_first", "durable_first", "local_only", "cloud_only"],
                            "description": "Preferred data source. Missing or unsatisfied providers are reported in coverage warnings."
                        },
                        "include_context": {
                            "type": "boolean",
                            "description": "Request persisted or visible context facts when a provider is available; missing providers are reported."
                        },
                        "question": {
                            "type": "string",
                            "description": "Concrete question to guide the analysis. Use this instead of a question facet."
                        },
                        "last_n": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "default": 20,
                            "description": "Evidence budget for recent events or decisions. This is not a horizon alias."
                        }
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "get_agent_info",
                "description": "Return the current Astra agent identity and capability summary. Use dimension='capability' to inspect which tools are actually available under the current workspace, executor, runtime, and policy binding.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "dimension": {
                            "type": "string",
                            "enum": ["identity", "capability", "all"],
                            "description": "Information slice to return. Defaults to all."
                        }
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "tool_search",
                "description":
                    "Activate deferred tools by explicit catalog name. `select:NAME[,NAME]` \
                     returns compact callable shape and queues schemas for the next request. \
                     Natural-language intent matching is deliberately unsupported.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "Explicit `select:NAME` or `select:NAME1,NAME2` activation."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }
            }
        }),
        // ── Notify (proactive notification for gateways) ─────────────────
        json!({
            "type": "function",
            "function": {
                "name": "notify",
                "description": "Send a user notification or status update. Use notification_type='proactive' only for push-worthy updates; CLI renders both modes inline.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": {"type": "string", "description": "Notification content"},
                        "notification_type": {"type": "string", "enum": ["normal","proactive"], "description": "Routing hint for gateway. 'proactive' = push even if user isn't looking at chat."}
                    },
                    "required": ["message"]
                }
            }
        }),
        // ── Ask user (interactive clarification) ─────────────────────────
        json!({
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user structured questions when a decision is needed. Supports 1-6 questions, headers, options, multi_select, and allow_freeform. Use for clarifications.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "Brief context shown above the questionnaire (dimmed)"},
                        "questions": {
                            "type": "array",
                            "description": "1-6 questions to present in the ask_user questionnaire.",
                            "minItems": 1,
                            "maxItems": 6,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "header": {"type": "string", "description": "Very short tab label, e.g. 'Frontend' or 'Database'. If omitted, the UI derives one from the question."},
                                    "question": {"type": "string", "description": "The focused question to ask for this tab."},
                                    "options": {"type": "array", "items": {"anyOf": [
                                        {"type": "string"},
                                        {"type": "object", "properties": {
                                            "label": {"type": "string", "description": "Option label shown in the picker"},
                                            "description": {"type": "string", "description": "Short explanatory text shown next to the option"},
                                            "preview": {"type": "string", "description": "Optional preview text shown in a side-by-side preview panel for single-select questions."}
                                        }, "required": ["label"]}
                                    ]}, "description": "Usually 2-9 options for this question. Do not include Other; use allow_freeform. May be omitted for a pure freeform question."},
                                    "multi_select": {"type": "boolean", "description": "Whether the user may select multiple options for this question."},
                                    "allow_freeform": {"type": "boolean", "description": "Whether the UI should add an automatic Other/freeform path for this question. Defaults to true."}
                                },
                                "required": ["question"]
                            }
                        }
                    },
                    "required": ["questions"]
                }
            }
        }),
        // ── background task control ─────────────────────────────────
        // Typed control surface for background tasks. Starting shell work stays
        // on Bash / Ctrl+B and local agents stay on agent(); control actions
        // use explicit tools rather than a generic action union.
        json!({
            "type": "function",
            "function": {
                "name": "task_output",
                "description": "Observe one specific typed background task and return its task kind/status. For an append-only shell task, omitting offset returns one bounded latest-tail status snapshot; do this at most once per task in a turn and do not chase live progress with a cursor. For a terminal shell task, especially a failure, set pattern to search the captured output with bounded context instead of reading its files through Bash; terminal diagnostics remain available after a status snapshot. Agent-result tasks may return a cursor when their semantic result is larger than one bounded response. Set block=true once when the user explicitly asks to wait: the runtime waits for terminal completion, required input, or timeout without spending additional model rounds. Supply an explicit offset only when the user asked to read historical shell output, then use next_offset for bounded pagination. Requires the exact task_id so the model and UI refer to the same task.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Background task id, such as bg-shell-3 or a local agent id."
                        },
                        "block": {
                            "type": "boolean",
                            "description": "Wait inside the runtime for terminal task status, required input, or timeout. Default false. Set true once only when the user explicitly asks to wait; ordinary output growth does not wake the model."
                        },
                        "offset": {
                            "type": "integer",
                            "minimum": 0,
                            "description": "Explicit output cursor. For shell tasks, omit it for one current latest-tail status snapshot and set it only when the user asked to page historical output. For bounded agent results, reuse the returned next_offset to continue the semantic result."
                        },
                        "pattern": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 512,
                            "description": "Single-line literal text to find in a terminal shell task's captured stdout/stderr. Returns bounded matching lines with context. Use an exact failing test, error, or panic fragment from the terminal summary. Cannot be combined with block or offset. A failed status alone is not evidence that a test is flaky."
                        },
                        "context_lines": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 20,
                            "description": "Lines before and after each literal match. Used only with pattern. Default 3, max 20."
                        },
                        "max_bytes": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 65536,
                            "description": "Maximum bytes to return from the current offset. Default 8192, max 65536."
                        },
                        "timeout_ms": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 300000,
                            "description": "Max ms to wait when block=true, and max registry response wait when block=false. Default 30000, max 300000."
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_stop",
                "description": "Request cancellation of a running typed background task by id. Returns structured ok/status/terminal fields; stop_requested is an accepted request and a later terminal notification closes the lifecycle. Use for stuck shell tasks, waiting-for-input tasks, local agents, or tasks the user explicitly wants cancelled. Requires an exact task_id; does not stop the most recent task implicitly.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "task_id": {
                            "type": "string",
                            "description": "Background task id to stop, such as bg-shell-3 or a local agent id."
                        }
                    },
                    "required": ["task_id"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "task_list",
                "description": "List known typed background tasks for this session with kind, status, and ids. Use when you need to discover which background task to inspect or stop.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "include_terminal": {
                            "type": "boolean",
                            "description": "Include recently completed, failed, or killed tasks. Default true."
                        }
                    }
                }
            }
        }),
        // ── enter_plan_mode ─────────────────────────────────────────
        // Top-level sentinel tool that flips the session into plan
        // mode. Promoted from the buried `session.enter_plan` action
        // in 2026-05 because the model rarely picked the sub-action
        // — the reference agent's dedicated `EnterPlanMode` tool is the
        // reference. While in plan mode, write tools (str_replace,
        // write_file, bash, git commit, …) are denied at the
        // permission gate; already-visible read tools stay available.
        // Exit via `exit_plan_mode` — that's the only unlock path.
        json!({
            "type": "function",
            "function": {
                "name": "enter_plan_mode",
                "description": "Enter plan mode for non-trivial work that needs design before code. While in plan mode, mutating tools are blocked at the permission gate and only already-visible read/control tools remain usable. Author the plan, then call `exit_plan_mode` with the markdown for user approval.\n\
        \n\
        ## When to Use This Tool\n\
        Use plan mode when user alignment before edits materially reduces risk:\n\
        - Multiple reasonable implementation approaches exist and the choice affects architecture, data flow, permissions, public API, or persistence.\n\
        - Requirements are unclear enough that exploration should precede a concrete implementation proposal.\n\
        - The work is high-impact or hard to unwind, such as schema changes, auth/security behavior, cross-cutting refactors, or large migrations.\n\
        - The user explicitly wants a plan, design review, or approval before implementation.\n\
        \n\
        When you enter plan mode:\n\
        1. Edits are blocked by design.\n\
        2. Explore only with read tools that are already visible in the current turn, and identify existing patterns to follow.\n\
        3. Produce executable leaf steps: each step should map to one concrete artifact, API surface, or validation target.\n\
        4. Avoid umbrella steps like \"build the whole system\" when code, API, UI, and verification are separate outcomes.\n\
        5. Call `exit_plan_mode(plan='<markdown>')` to submit the plan for user approval. Approval is produced by the UI/control plane, not by model-supplied tool arguments.\n\
        \n\
        ## When NOT to Use This Tool\n\
        Do not enter plan mode when normal execution is clearer:\n\
        - Single-line / few-line fixes (typos, obvious bugs).\n\
        - User gave specific step-by-step instructions — just do them.\n\
        - The required read tools are not visible; answer from conversation context or say the capability is unavailable instead.\n\
        - Pure research / read-only exploration with no implementation step (use `agent` with explore type instead when that tool is visible).\n\
        - The work is < 3 files and the approach is obvious.\n\
        \n\
        Important: `exit_plan_mode` is the ONLY way to leave plan mode. Do not use `ask_user` to ask \"is the plan ready?\" — `exit_plan_mode` itself surfaces the plan for approval.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "goal": {
                            "type": "string",
                            "description": "Optional one-line goal label that surfaces in the TUI plan-mode banner. Defaults to a placeholder if omitted."
                        }
                    },
                    "required": []
                }
            }
        }),
        // ── exit_plan_mode ──────────────────────────────────────────
        // Companion to enter_plan_mode. Surfaces the proposed plan to
        // the user for approval, lifts the write-tool guard on
        // success. The durable plan remains its own record; the task board
        // is an explicit execution checklist, never a copied plan tree.
        json!({
            "type": "function",
            "function": {
                "name": "exit_plan_mode",
                "description": "Submit the plan for user approval. The `plan` argument is a markdown string (numbered list, nested bullets ok) that the user reads and either approves or rejects in the trusted UI. The model cannot approve its own plan; approval unlocks writes only after the UI/control plane returns the user's decision. The approved plan remains a durable plan record; use the task board only when an execution checklist materially helps.\n\
        \n\
        ## Plan structure (what makes a good plan)\n\
        - Numbered list of concrete, executable leaf steps — each step maps to ONE artifact, API surface, or validation target.\n\
        - Each step includes: what files to touch, what to change, and the acceptance criteria.\n\
        - Avoid umbrella phases like \"build the system\" — split into scaffold → implement → test → verify.\n\
        - Prefer 3-7 steps for most work; >10 steps signals over-decomposition.\n\
        \n\
        ## Important\n\
        - Do NOT call this tool to ask 'is the plan ready?' — that's exactly what THIS tool does. It inherently requests approval.\n\
        - Pass the FULL plan as a single markdown string in `plan`. The user sees this verbatim.\n\
        - Only call this when the plan is concrete and unambiguous. If you have unresolved decisions, use `ask_user` first.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "plan": {
                            "type": "string",
                            "description": "The plan markdown to present for approval. Numbered list of steps; nested bullets ok. The user reads this verbatim."
                        }
                    },
                    "required": ["plan"]
                }
            }
        }),
    ]
}

#[cfg(test)]
#[allow(dead_code, unused_imports, clippy::empty_line_after_doc_comments)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn built_in_schema_construction_fits_default_tokio_worker_stack() {
        // Call the uncached constructor directly: another schema test may
        // already have initialized the process-global validation index.
        let schemas = std::thread::Builder::new()
            .name("fresh-built-in-schemas".to_string())
            .stack_size(2 * 1024 * 1024)
            .spawn(all_tool_schemas_core)
            .expect("spawn fresh schema constructor")
            .join()
            .expect("fresh schema construction must fit a default Tokio worker stack");
        assert!(find_schema(&schemas, "tool_search").is_some());
        assert!(find_schema(&schemas, "agent").is_some());
    }

    fn schema_names(schemas: &[Value]) -> Vec<&str> {
        schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .collect()
    }

    fn schema_token_cost(schema: &Value) -> usize {
        serde_json::to_string(schema)
            .expect("schema must serialize")
            .len()
            .div_ceil(4)
    }

    #[test]
    fn action_surface_projection_is_declarative_and_tool_agnostic() {
        let mut schemas = vec![json!({
            "type": "function",
            "function": {
                "name": "future_consolidated_tool",
                "description": "shared",
                "parameters": {
                    "type": "object",
                    "x-astra-action-surfaces": {
                        "shared": ["local", "server"],
                        "local_only": ["local"]
                    },
                    "x-astra-surface-descriptions": {"server": "server projection"},
                    "x-astra-surface-discovery-summaries": {"server": "shared only"},
                    "properties": {
                        "action": {"type": "string", "enum": ["shared", "local_only"]},
                        "common": {"type": "string"},
                        "local_arg": {"type": "string"}
                    },
                    "x-astra-per-action-required": {
                        "shared": ["common"],
                        "local_only": ["local_arg"]
                    },
                    "x-astra-per-action-allowed": {
                        "shared": ["action", "common"],
                        "local_only": ["action", "local_arg"]
                    }
                }
            }
        })];

        project_action_schemas_for_surface(&mut schemas, "server");

        let schema = &schemas[0];
        assert_eq!(schema["function"]["description"], "server projection");
        assert_eq!(
            schema["function"]["parameters"]["properties"]["action"]["enum"],
            json!(["shared"])
        );
        assert!(
            schema["function"]["parameters"]["properties"]
                .get("local_arg")
                .is_none()
        );
        assert!(
            schema["function"]["parameters"][PER_ACTION_ALLOWED_KEY]
                .get("local_only")
                .is_none()
        );
        assert_eq!(
            schema["function"]["parameters"]["x-astra-discovery-summary"],
            "shared only"
        );
        assert!(
            schema["function"]["parameters"]
                .get(ACTION_SURFACES_KEY)
                .is_none()
        );
    }

    fn required_fields(schema: &Value) -> Vec<String> {
        schema
            .pointer("/function/parameters/required")
            .and_then(Value::as_array)
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    // execute_code has been deleted. The only hallucination-prevention
    // concern now is ensuring run_script is advertised on Unix, and that
    // `execute_code` is NOT in the schema list (so the model doesn't
    // hallucinate it).

    // ── agent tool: structured foreground contract ────────────────────────

    #[test]
    fn agent_schema_does_not_expose_model_background_parameter() {
        let schemas = all_tool_schemas();
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let props = agent
            .get("function")
            .and_then(|f| f.get("parameters"))
            .and_then(|p| p.get("properties"))
            .expect("agent must expose parameters.properties");
        assert!(
            props.get("run_in_background").is_none(),
            "foreground/background is a user control; the model must not choose scheduling policy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_background_bash_contract_is_executor_projected() {
        let mut schemas = all_tool_schemas();
        let base = find_schema(&schemas, "bash").expect("bash schema must exist");
        let base_props = &base["function"]["parameters"]["properties"];
        assert!(base_props.get("run_in_background").is_none());
        assert!(base_props.get("ready_check").is_none());
        assert!(base_props.get("background_ttl").is_none());
        assert!(
            base["function"]["description"]
                .as_str()
                .unwrap()
                .contains("no process-persistence guarantee")
        );

        enable_managed_background_bash_schema(&mut schemas);
        let bash = find_schema(&schemas, "bash").expect("bash schema must exist");
        let props = &bash["function"]["parameters"]["properties"];
        assert_eq!(props["run_in_background"]["type"], "boolean");
        assert_eq!(props["ready_check"]["type"], "string");
        assert_eq!(props["background_ttl"]["maximum"], 3600);
        let description = bash["function"]["description"].as_str().unwrap();
        assert!(description.contains("Self-daemonizing"));
        assert!(description.contains("no persistence guarantee"));
    }

    #[cfg(not(unix))]
    #[test]
    fn managed_background_bash_contract_is_not_projected_when_unsupported() {
        let mut schemas = all_tool_schemas();
        enable_managed_background_bash_schema(&mut schemas);
        let bash = find_schema(&schemas, "bash").expect("bash schema must exist");
        let props = &bash["function"]["parameters"]["properties"];
        assert!(props.get("run_in_background").is_none());
        assert!(props.get("ready_check").is_none());
        assert!(props.get("background_ttl").is_none());
    }

    #[test]
    fn agent_fanout_schema_exposes_atomic_group_contract() {
        let schemas = all_tool_schemas();
        let fanout = find_schema(&schemas, "agent_fanout").expect("agent_fanout schema must exist");
        let description = fanout["function"]["description"]
            .as_str()
            .expect("fanout description");
        assert!(
            description.contains(
                "user request or loaded workflow contains an explicit topology directive"
            )
        );
        assert!(description.contains("requirements alone do not imply extra agents"));
        assert!(description.contains("omit `max_turns`"));
        let params = &fanout["function"]["parameters"];

        assert_eq!(params["additionalProperties"], false);
        assert_eq!(
            params["properties"]["action"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            crate::agent_tool_contract::AGENT_FANOUT_ACTIONS
        );
        assert_eq!(
            params["x-astra-per-action-required"]["start"],
            json!(["target_count", "slots"])
        );
        assert_eq!(
            params["x-astra-per-action-required"]["get_results"],
            json!(["group_id"])
        );
        assert_eq!(
            params["x-astra-per-action-required"]["stop_group"],
            json!(["group_id"])
        );
        assert!(params["properties"].get("offset").is_some());
        assert_eq!(params["properties"]["max_bytes"]["maximum"], 65536);
        assert_eq!(
            params["properties"]["slots"]["items"]["required"],
            json!(["description", "prompt"])
        );
        assert!(params["properties"].get("run_in_background").is_none());
        let slot_props = &params["properties"]["slots"]["items"]["properties"];
        assert!(
            slot_props.get("id").is_some(),
            "fanout slots must expose the canonical caller-facing identity field"
        );
        assert!(slot_props.get("slot_id").is_none());
        assert_eq!(
            slot_props["description"]["maxLength"],
            crate::agent_tool_contract::AGENT_FANOUT_SLOT_DESCRIPTION_MAX_CHARS
        );
        assert_eq!(
            slot_props["prompt"]["maxLength"],
            crate::agent_tool_contract::AGENT_FANOUT_SLOT_PROMPT_MAX_CHARS
        );
        let description = fanout["function"]["description"]
            .as_str()
            .expect("fanout description");
        assert!(description.contains("allowed_tools"));
        assert!(description.contains("no `tools` field"));
        assert!(
            fanout["function"]["description"]
                .as_str()
                .is_some_and(|description| description
                    .to_ascii_lowercase()
                    .contains("never paste file contents")),
            "the advertised contract must prevent large diff/tool-output embedding at generation time"
        );
        let description = fanout["function"]["description"]
            .as_str()
            .expect("fanout description");
        assert!(
            description.contains("only tools exposed in a child's own tool surface are usable")
                && description.contains("workspace provider is unavailable"),
            "fanout must distinguish inherited bindings from actual provider availability"
        );
        assert!(
            !description.contains("Children share the bound workspace")
                && !slot_props["prompt"]["description"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains("shares the workspace")),
            "fanout must not promise a workspace that the current provider binding cannot supply"
        );
        assert!(
            slot_props.get("name").is_none(),
            "fanout slots should not expose spawn mailbox names as slot identity"
        );
    }

    #[test]
    fn agent_schema_structurally_owns_identity_fields_by_action() {
        let schemas = all_tool_schemas();
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let agent_description = agent["function"]["description"]
            .as_str()
            .expect("agent description");
        assert!(agent_description.contains("initial_task"));
        assert!(agent_description.contains("next_task"));
        assert!(agent_description.contains("only when neither response supplied an assignment"));
        assert!(
            agent_description
                .contains("Treat an assigned task's expected result as its stop boundary")
        );
        assert!(
            !agent_description.contains("After Work exists, use `run_next_work_item({})`"),
            "the agent and Work schemas must not give contradictory task-claim instructions"
        );
        let params = &agent["function"]["parameters"];
        assert_eq!(params["additionalProperties"], false);
        assert_eq!(
            params["properties"]["action"]["enum"]
                .as_array()
                .expect("agent action enum")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            crate::agent_tool_contract::AGENT_ACTIONS
        );

        let spawn_with_runtime_id = validate_tool_arguments(
            "agent",
            &json!({
                "action": "spawn",
                "description": "Review runtime",
                "prompt": "Review the runtime",
                "agent_id": "invented"
            }),
        )
        .unwrap_err();
        assert_eq!(
            spawn_with_runtime_id.issues,
            vec!["field(s) not allowed for action `spawn`: agent_id"]
        );

        validate_tool_arguments(
            "agent",
            &json!({
                "action": "spawn",
                "description": "Probe inherited prefix",
                "prompt": "Return the probe result",
                "max_output_tokens": 256,
                "inherit_prefix": {"required": false}
            }),
        )
        .expect("advertised prefix-inheritance fields must match the runtime spawn input");
        validate_tool_arguments(
            "agent",
            &json!({
                "action": "spawn",
                "description": "Implement canonical task",
                "prompt": "Implement and verify the assigned task",
                "work_item": {"item_id": "task-1", "item_revision": 2}
            }),
        )
        .expect("typed WorkItem assignment must match the runtime spawn input");
        assert_eq!(
            params["properties"]["work_item"]["additionalProperties"],
            false
        );
        let work_item_description = params["properties"]["work_item"]["description"]
            .as_str()
            .expect("WorkItem assignment description");
        assert!(work_item_description.contains("start_work or inspect_work_plan"));
        assert!(work_item_description.contains("server verifies current Work membership"));
        assert_eq!(
            params["properties"]["inherit_prefix"]["additionalProperties"],
            false
        );

        let result_with_mailbox_name = validate_tool_arguments(
            "agent",
            &json!({"action": "get_result", "agent_id": "runtime-id", "name": "mailbox"}),
        )
        .unwrap_err();
        assert_eq!(
            result_with_mailbox_name.issues,
            vec!["field(s) not allowed for action `get_result`: name"]
        );
    }

    #[test]
    fn start_work_schema_exposes_a_small_server_owned_task_list() {
        let schemas = all_tool_schemas();
        let schema = find_schema(&schemas, "start_work").expect("start_work schema");
        let description = schema["function"]["description"]
            .as_str()
            .expect("start_work description");
        assert!(description.contains("initial_task"));
        assert!(description.contains("Count user acceptance units"));
        assert!(description.contains("each owes its own payload or evidence"));
        assert!(description.contains("execute it directly"));
        assert!(description.contains("extends that branch without creating a second Work"));
        assert!(description.contains(
            "task identity, ordering, and execution dependencies are assigned by the server"
        ));
        assert_eq!(
            required_fields(schema),
            vec![
                "goal".to_string(),
                "activation".to_string(),
                "tasks".to_string()
            ]
        );
        let parameters = &schema["function"]["parameters"];
        assert!(
            parameters["properties"]["tasks"]["description"]
                .as_str()
                .expect("task list description")
                .contains("server owns IDs and dependency mechanics")
                && parameters["properties"]["tasks"]["items"]["properties"]
                    .get("item_id")
                    .is_none()
                && parameters["properties"]["tasks"]["items"]["properties"]
                    .get("kind")
                    .is_none(),
            "task schema must not ask the model for identity, kind, or dependency fields"
        );
        assert_eq!(
            parameters["properties"]["tasks"]["items"]["required"],
            json!(["objective", "expected_result"]),
        );
        validate_tool_arguments(
            "start_work",
            &json!({
                "goal": "Ship a verified change",
                "activation": "start",
                "tasks": [{
                    "objective": "Verify the current behavior",
                    "expected_result": "Reproducible evidence of the current behavior"
                }]
            }),
        )
        .expect("exact start input");
        validate_tool_arguments(
            "start_work",
            &json!({
                "goal": "Collect two evidence tracks before synthesis",
                "activation": "defer",
                "tasks": [
                    {
                        "objective": "Inspect the first evidence source",
                        "expected_result": "A cited finding"
                    },
                    {
                        "objective": "Synthesize the evidence",
                        "expected_result": "A concise conclusion"
                    }
                ]
            }),
        )
        .expect("ordered tasks are the initial Work contract");
        for invalid in [
            json!({}),
            json!({"goal": "Ship it", "activation": "start", "tasks": []}),
            json!({
                "goal": "Ship it",
                "activation": "start",
                "tasks": [{
                    "objective": "Do it",
                    "expected_result": "It works"
                }],
                "dependencies": []
            }),
            json!({
                "goal": "Ship it",
                "activation": "start",
                "tasks": [{
                    "objective": "Do it",
                    "expected_result": "It works",
                    "status": "completed"
                }]
            }),
        ] {
            assert!(validate_tool_arguments("start_work", &invalid).is_err());
        }
    }

    #[test]
    fn settlement_summary_is_explicitly_non_authoritative() {
        let schemas = all_tool_schemas();
        let schema = find_schema(&schemas, "settle_work_item").expect("settle schema");
        let description = schema["function"]["description"]
            .as_str()
            .expect("settle description");
        assert!(description.contains("derived progress note"));
        assert!(description.contains("not an authoritative evidence source"));
        assert!(description.contains("direct tool/artifact sources"));
        assert!(description.contains("literal gap check"));
        assert!(description.contains("index/home page"));
        assert!(description.contains("every required observed payload field"));
        assert!(description.contains("None of these outcomes means cancelled"));
        assert!(description.contains("declaration_state=cancelled"));
    }

    #[test]
    fn run_next_work_item_schema_leaves_task_selection_to_canonical_work() {
        let schemas = all_tool_schemas();
        let schema =
            find_schema(&schemas, "run_next_work_item").expect("run_next_work_item schema");
        let description = schema["function"]["description"]
            .as_str()
            .expect("description");
        assert!(description.contains("expected_result is the attempt's completion boundary"));
        assert!(description.contains("do not broaden"));
        assert!(description.contains("only when no assignment was returned"));
        assert!(description.contains("initial_task"));
        assert!(description.contains("next_task"));
        assert!(
            !description.contains("Use after start_work and after each settlement"),
            "run-next must not contradict direct server-issued assignments"
        );
        assert!(required_fields(schema).is_empty());
        validate_tool_arguments("run_next_work_item", &json!({}))
            .expect("canonical Work selects the next task");
        assert!(
            validate_tool_arguments(
                "run_next_work_item",
                &json!({"item_id": "model-must-not-select-a-task"}),
            )
            .is_err()
        );
    }

    #[test]
    fn settle_work_item_schema_exposes_only_typed_attempt_facts() {
        let schemas = all_tool_schemas();
        let schema = find_schema(&schemas, "settle_work_item").expect("settlement schema");
        assert_eq!(
            required_fields(schema),
            vec!["outcome".to_string(), "summary".to_string()]
        );
        validate_tool_arguments(
            "settle_work_item",
            &json!({
                "outcome": "blocked",
                "summary": "Network tool is unavailable",
                "blocker_kind": "capability_unavailable",
                "unavailable_capabilities": ["web_fetch"]
            }),
        )
        .expect("typed blocked settlement");
        assert!(
            validate_tool_arguments(
                "settle_work_item",
                &json!({
                    "outcome": "completed",
                    "summary": "free-text completion is not a delivery outcome"
                })
            )
            .is_err()
        );
        assert!(
            validate_tool_arguments(
                "settle_work_item",
                &json!({"outcome": "delivered", "summary": "done", "run_id": "invented"})
            )
            .is_err(),
            "the model cannot choose Work, item, or attempt identity"
        );
    }

    #[test]
    fn typed_background_task_schemas_replace_job_public_contract() {
        let schemas = all_tool_schemas();
        assert!(
            find_schema(&schemas, "job").is_none()
                && find_schema(&schemas, "task_output").is_some()
                && find_schema(&schemas, "task_stop").is_some()
                && find_schema(&schemas, "task_list").is_some(),
            "model-facing schema must expose typed background task tools, not generic job"
        );
        let output = find_schema(&schemas, "task_output").expect("task_output schema");
        assert_eq!(required_fields(output), vec!["task_id".to_string()]);
        assert_eq!(
            output["function"]["parameters"]["properties"]["block"]["type"],
            "boolean"
        );
    }

    #[test]
    fn legacy_task_board_is_absent_from_the_model_surface() {
        let schemas = all_tool_schemas();
        assert!(
            find_schema(&schemas, "task").is_none()
                && find_schema(&schemas, "task_board").is_none(),
            "model-facing planning must have one typed Work authority, not a legacy task mutation tool"
        );
        assert!(find_schema(&schemas, "inspect_work_plan").is_some());
        assert!(find_schema(&schemas, "propose_work_plan").is_some());
    }

    #[test]
    fn work_planning_schemas_are_strict_typed_and_bounded() {
        let schemas = all_tool_schemas();
        let inspect = find_schema(&schemas, "inspect_work_plan").expect("inspect schema");
        assert_eq!(
            inspect["function"]["parameters"]["additionalProperties"],
            false
        );
        assert!(required_fields(inspect).is_empty());
        assert!(validate_tool_arguments("inspect_work_plan", &json!({})).is_ok());
        assert!(
            validate_tool_arguments(
                "inspect_work_plan",
                &json!({
                    "context_id": format!("work-plan-context:{}", "a".repeat(64)),
                    "item_offset": 8,
                    "dependency_offset": 128
                })
            )
            .is_ok()
        );
        assert!(
            validate_tool_arguments("inspect_work_plan", &json!({"item_offset": 257})).is_err()
        );
        assert!(
            validate_tool_arguments("inspect_work_plan", &json!({"query": "anything"})).is_err()
        );

        let propose = find_schema(&schemas, "propose_work_plan").expect("propose schema");
        let parameters = &propose["function"]["parameters"];
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(
            required_fields(propose),
            vec![
                "context_id",
                "reason",
                "additions",
                "revisions",
                "dependencies",
                "dependency_removals"
            ]
        );
        assert_eq!(parameters["properties"]["additions"]["maxItems"], 64);
        assert_eq!(parameters["properties"]["dependencies"]["maxItems"], 256);
        assert_eq!(
            parameters["properties"]["additions"]["items"]["properties"]["kind"]["enum"],
            json!(["milestone", "task"])
        );
        assert_eq!(
            parameters["properties"]["additions"]["items"]["additionalProperties"],
            false
        );
        assert!(
            propose["function"]["description"]
                .as_str()
                .is_some_and(
                    |description| description.contains("same durable unit of work")
                        && description.contains("fresh item_id")
                )
        );
        assert!(
            parameters["properties"]["additions"]["items"]["properties"]["item_id"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("Never reuse"))
        );
        let valid = json!({
            "context_id": format!("work-plan-context:{}", "a".repeat(64)),
            "reason": "Add the next independently verifiable task",
            "additions": [{
                "item_id": "task-1",
                "kind": "task",
                "objective": "Implement the bounded primitive",
                "expected_result": "The primitive is deterministically verified"
            }],
            "revisions": [],
            "dependencies": [],
            "dependency_removals": []
        });
        validate_tool_arguments("propose_work_plan", &valid)
            .unwrap_or_else(|error| panic!("valid typed Work proposal rejected: {error}"));
        let mut unknown = valid.clone();
        unknown["guess"] = Value::Bool(true);
        assert!(validate_tool_arguments("propose_work_plan", &unknown).is_err());
        let mut empty = valid;
        empty["additions"] = json!([]);
        assert!(
            validate_tool_arguments("propose_work_plan", &empty).is_ok(),
            "cross-array non-empty admission is enforced by the typed runtime boundary"
        );

        let inspect_criteria =
            find_schema(&schemas, "inspect_work_criteria").expect("criteria inspect schema");
        assert!(required_fields(inspect_criteria).is_empty());
        assert!(validate_tool_arguments("inspect_work_criteria", &json!({})).is_ok());
        assert!(
            validate_tool_arguments(
                "inspect_work_criteria",
                &json!({"context_id": format!("work-plan-context:{}", "b".repeat(64)), "offset": 4})
            )
            .is_ok()
        );
        assert!(validate_tool_arguments("inspect_work_criteria", &json!({"offset": 129})).is_err());

        let propose_criteria =
            find_schema(&schemas, "propose_work_criteria").expect("criteria proposal schema");
        assert_eq!(
            required_fields(propose_criteria),
            vec!["context_id", "members"]
        );
        assert_eq!(
            propose_criteria["function"]["parameters"]["properties"]["members"]["maxItems"],
            128
        );
        let criteria = json!({
            "context_id": format!("work-plan-context:{}", "b".repeat(64)),
            "members": [
                {"member_kind": "existing", "criterion_id": "existing-check", "revision": 1},
                {
                    "member_kind": "new",
                    "criterion_id": "tests-pass",
                    "definition": {
                        "kind": "test_check",
                        "statement": "Relevant tests pass.",
                        "command": "cargo test -p astra-runtime"
                    }
                }
            ]
        });
        validate_tool_arguments("propose_work_criteria", &criteria)
            .unwrap_or_else(|error| panic!("valid criteria proposal rejected: {error}"));
        let mut wrong_variant = criteria.clone();
        wrong_variant["members"][0]["definition"] =
            json!({"kind": "human_review", "statement": "Review it."});
        assert!(validate_tool_arguments("propose_work_criteria", &wrong_variant).is_err());
        let mut unknown = criteria;
        unknown["members"][1]["guess"] = json!(true);
        assert!(validate_tool_arguments("propose_work_criteria", &unknown).is_err());
    }

    #[test]
    fn memory_schema_stays_compact() {
        let schemas = all_tool_schemas();
        let memory = find_schema(&schemas, "memory").expect("memory schema must exist");
        let memory_tokens = schema_token_cost(memory);

        assert!(
            memory_tokens <= 700,
            "memory schema regressed to {memory_tokens} tokens; keep it compact"
        );
    }

    #[test]
    fn memory_importance_contract_is_numeric_and_bounded() {
        let schemas = all_tool_schemas();
        let memory = find_schema(&schemas, "memory").expect("memory schema must exist");
        let importance = &memory["function"]["parameters"]["properties"]["importance"];

        assert_eq!(importance["type"], "number");
        assert_eq!(importance["minimum"], 0.0);
        assert_eq!(importance["maximum"], 1.0);
        assert!(
            importance["description"]
                .as_str()
                .is_some_and(|description| description.contains("do not use labels"))
        );
    }

    #[test]
    fn always_load_high_frequency_descriptions_stay_compact() {
        let schemas = all_tool_schemas();
        for (name, max_len) in [
            ("bash", 180usize),
            ("str_replace", 180),
            ("git", 140),
            ("memory", 120),
            ("ask_user", 180),
            ("notify", 180),
            ("tool_search", 240),
        ] {
            let schema = find_schema(&schemas, name).expect("schema must exist");
            let desc = schema["function"]["description"].as_str().unwrap_or("");
            assert!(
                desc.len() <= max_len,
                "{name} description regressed to {} chars; max {max_len}: {desc}",
                desc.len()
            );
        }
    }

    #[test]
    fn notify_always_load_incremental_schema_cost_is_quantified() {
        let schemas = all_tool_schemas();
        let notify = find_schema(&schemas, "notify").expect("notify schema must exist");
        let notify_tokens = schema_token_cost(notify);
        const EXPECTED_NOTIFY_TOKENS: usize = 126;
        const NOTIFY_ALWAYS_LOAD_TOKEN_CEILING: usize = 180;
        assert_eq!(
            notify_tokens, EXPECTED_NOTIFY_TOKENS,
            "notify always-load cost changed; update docs/design/skills-and-tools.md if intentional"
        );
        assert!(
            notify_tokens <= NOTIFY_ALWAYS_LOAD_TOKEN_CEILING,
            "notify always-load cost is {notify_tokens} tokens; keep the status-update primitive compact"
        );
    }

    #[test]
    fn github_schema_action_enum_matches_executor_contract() {
        let schemas = all_tool_schemas();
        let github = find_schema(&schemas, "github").expect("github schema");
        let actions = github["function"]["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .expect("github action enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(actions, crate::github_tool_contract::GITHUB_ACTIONS);
    }

    #[test]
    fn git_schema_action_enum_matches_executor_contract() {
        let schemas = all_tool_schemas();
        let git = find_schema(&schemas, "git").expect("git schema");
        let actions = git["function"]["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .expect("git action enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(actions, crate::git_tool_contract::GIT_ACTIONS);
    }

    #[test]
    fn git_schema_exposes_executor_stat_only_capability() {
        let schemas = all_tool_schemas();
        let git = find_schema(&schemas, "git").expect("git schema");
        assert_eq!(
            git.pointer("/function/parameters/properties/stat_only/type")
                .and_then(Value::as_str),
            Some("boolean")
        );
        validate_tool_arguments("git", &json!({"action": "diff", "stat_only": true}))
            .expect("advertised stat_only diff must validate");
    }

    #[test]
    fn git_schema_and_executor_share_the_blame_contract() {
        let schemas = all_tool_schemas();
        let git = find_schema(&schemas, "git").expect("git schema");
        assert_eq!(
            git.pointer("/function/parameters/x-astra-per-action-required/blame/0")
                .and_then(Value::as_str),
            Some("path")
        );
        validate_tool_arguments(
            "git",
            &json!({
                "action": "blame",
                "path": "src/lib.rs",
                "start_line": 10,
                "end_line": 20
            }),
        )
        .expect("advertised blame request must validate");
    }

    #[test]
    fn git_schema_exposes_canonical_multi_path_diff_filters() {
        let schemas = all_tool_schemas();
        let git = find_schema(&schemas, "git").expect("git schema");
        assert_eq!(
            git.pointer("/function/parameters/properties/paths/type")
                .and_then(Value::as_str),
            Some("array")
        );
        validate_tool_arguments(
            "git",
            &json!({
                "action": "diff",
                "base_ref": "main",
                "ref": "HEAD",
                "paths": ["src/one.rs", "src/two.rs"]
            }),
        )
        .expect("advertised canonical multi-path diff filters must validate");
    }

    #[test]
    fn memory_schema_action_enum_matches_executor_contract() {
        let schemas = all_tool_schemas();
        let memory = find_schema(&schemas, "memory").expect("memory schema");
        let actions = memory["function"]["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .expect("memory action enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(actions, crate::memory_tool_contract::MEMORY_ACTIONS);
    }

    #[test]
    fn introspect_schema_describes_live_observation_surface() {
        let schemas = all_tool_schemas();
        let introspect = find_schema(&schemas, "introspect").expect("introspect schema must exist");
        let params = &introspect["function"]["parameters"];
        let description = introspect["function"]["description"]
            .as_str()
            .expect("introspect description must be present");
        assert!(description.contains("recent live projection"));
        assert!(description.contains("use reflect"));
        let properties = introspect["function"]["parameters"]["properties"]
            .as_object()
            .expect("introspect parameters properties must be an object");
        assert!(
            properties["horizon"]["description"]
                .as_str()
                .expect("introspect horizon description")
                .contains("rather than failing")
        );
        assert_eq!(
            enum_values(&properties["horizon"]),
            vec![
                "now",
                "current_turn",
                "recent",
                "turn",
                "session",
                "cross_session"
            ],
            "introspect must accept semantic historical requests and label its live projection"
        );
        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "introspect must be strict so removed/legacy parameters do not leak back"
        );
        assert_eq!(
            enum_values(&properties["topic"]),
            vec!["overview", "runtime", "execution", "knowledge"],
            "introspect topic schema must expose only implemented top-level observation areas"
        );
        assert!(
            !enum_values(&properties["topic"]).contains(&"adaptation"),
            "introspect must not advertise premature adaptation topic"
        );
        assert_eq!(
            enum_values(&properties["facet"]),
            vec![
                "session",
                "overview",
                "recent",
                "errors",
                "trace",
                "volatile",
                "stall",
                "noise",
                "cache",
                "session_memory",
            ],
            "introspect facet schema must expose canonical leaf facets"
        );
        for key in [
            "topic",
            "facet",
            "depth",
            "horizon",
            "question",
            "source_policy",
            "include_context",
            "format",
            "artifact",
            "offset",
            "max_bytes",
        ] {
            assert!(
                properties.contains_key(key),
                "introspect schema should expose normalized observation parameter `{key}`"
            );
        }
        assert!(
            !properties.contains_key("subtopic")
                && !properties.contains_key("detail")
                && !properties.contains_key("focus"),
            "introspect schema must not expose removed legacy aliases"
        );
        assert_eq!(
            enum_values(&properties["depth"]),
            vec!["hint", "summary", "diagnostic", "forensic"],
            "introspect depth schema must expose canonical observation depths"
        );
        assert_eq!(
            enum_values(&properties["source_policy"]),
            vec![
                "auto",
                "live_only",
                "live_first",
                "durable_first",
                "local_only",
                "cloud_only",
            ],
            "introspect source_policy schema must not regress to old edge/server/cloud aliases"
        );
        assert_eq!(properties["max_bytes"]["maximum"], 65_536);
    }

    #[test]
    fn reflect_schema_describes_persisted_observation_surface() {
        let schemas = all_tool_schemas();
        let reflect = find_schema(&schemas, "reflect").expect("reflect schema must exist");
        let params = &reflect["function"]["parameters"];
        let properties = reflect["function"]["parameters"]["properties"]
            .as_object()
            .expect("reflect parameters properties must be an object");

        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "reflect must reject removed/legacy parameters"
        );
        assert_eq!(
            enum_values(&properties["topic"]),
            vec!["overview", "runtime", "execution", "knowledge"],
            "reflect topic schema must expose only implemented observation areas"
        );
        assert_eq!(
            enum_values(&properties["facet"]),
            vec![
                "overview",
                "performance",
                "errors",
                "tools",
                "trace",
                "context",
                "memory",
            ],
            "reflect facet schema must expose only implemented persisted evidence views"
        );
        for key in [
            "topic",
            "facet",
            "depth",
            "horizon",
            "source_policy",
            "include_context",
            "question",
            "last_n",
        ] {
            assert!(
                properties.contains_key(key),
                "reflect schema should expose normalized observation parameter `{key}`"
            );
        }
        assert!(
            !properties.contains_key("focus")
                && !enum_values(&properties["topic"]).contains(&"adaptation")
                && !enum_values(&properties["facet"]).contains(&"signals")
                && !enum_values(&properties["facet"]).contains(&"measurements")
                && !enum_values(&properties["facet"]).contains(&"question"),
            "reflect schema must not expose removed or premature adaptation/focus parameters"
        );
        assert_eq!(
            enum_values(&properties["depth"]),
            vec!["hint", "summary", "diagnostic", "forensic"],
            "reflect depth schema must expose canonical observation depths"
        );
        assert_eq!(
            properties["last_n"].get("minimum").and_then(Value::as_i64),
            Some(1),
            "reflect last_n must declare a lower evidence-budget bound"
        );
        assert_eq!(
            properties["last_n"].get("maximum").and_then(Value::as_i64),
            Some(100),
            "reflect last_n must declare an upper evidence-budget bound"
        );
    }

    fn enum_values(schema: &serde_json::Value) -> Vec<&str> {
        schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .expect("schema must expose enum array")
            .iter()
            .map(|value| value.as_str().expect("enum values must be strings"))
            .collect()
    }

    #[test]
    fn self_mod_session_state_top_level_schemas_exist() {
        let schemas = all_tool_schemas();
        for name in ["compress_context", "rollback_session_state"] {
            find_schema(&schemas, name)
                .expect("top-level schema must exist for ToolEngine routing");
        }
    }

    #[test]
    fn matrixone_top_level_schemas_exist() {
        let schemas = all_tool_schemas();
        for name in ["mo_query", "rollback_database_snapshots"] {
            find_schema(&schemas, name)
                .expect("top-level schema must exist for ToolEngine routing");
        }

        let mo_query = find_schema(&schemas, "mo_query").expect("mo_query schema");
        let required = mo_query["function"]["parameters"]["required"]
            .as_array()
            .expect("mo_query should declare required fields");
        assert!(
            required.iter().any(|value| value.as_str() == Some("sql")),
            "mo_query schema must require sql: {mo_query:?}"
        );
        let rollback =
            find_schema(&schemas, "rollback_database_snapshots").expect("rollback schema");
        let scopes = rollback["function"]["parameters"]["properties"]["scope"]["enum"]
            .as_array()
            .expect("rollback scope should have enum values")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();
        assert!(scopes.contains("snapshot"));
        assert!(scopes.contains("list"));
    }

    #[test]
    fn session_schema_exposes_only_lifecycle_and_history_actions() {
        let schemas = all_tool_schemas();
        let session = find_schema(&schemas, "session").expect("session schema");
        let actions = session["function"]["parameters"]["properties"]["action"]["enum"]
            .as_array()
            .expect("session action enum")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();

        assert_eq!(actions, crate::session_tool_contract::SESSION_ACTIONS);
        let props = session["function"]["parameters"]["properties"]
            .as_object()
            .expect("session properties");
        assert!(props.contains_key("path"));
        assert!(!props.contains_key("key"));
        assert!(!props.contains_key("tool"));
    }

    #[test]
    fn get_agent_info_schema_exposes_capability_dimension() {
        let schemas = all_tool_schemas();
        let get_agent_info =
            find_schema(&schemas, "get_agent_info").expect("get_agent_info schema must exist");
        let properties = get_agent_info["function"]["parameters"]["properties"]
            .as_object()
            .expect("get_agent_info properties must be an object");
        let dimension = properties
            .get("dimension")
            .expect("get_agent_info should expose dimension");
        let enum_values = dimension["enum"]
            .as_array()
            .expect("dimension should have enum values")
            .iter()
            .filter_map(Value::as_str)
            .collect::<std::collections::HashSet<_>>();

        assert!(enum_values.contains("identity"));
        assert!(enum_values.contains("capability"));
        assert!(enum_values.contains("all"));
    }

    #[test]
    fn execute_code_no_longer_present_in_schemas() {
        let schemas = all_tool_schemas();
        let names = schema_names(&schemas);
        assert!(
            !names.contains(&"execute_code"),
            "removed tool name execute_code must not leak into the schema list"
        );
    }

    // ── run_script schema visibility ──────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn run_script_visible_by_default_on_unix() {
        let schemas = all_tool_schemas();
        let names = schema_names(&schemas);
        assert!(
            names.contains(&"run_script"),
            "run_script must appear in the default schema list so the LLM can discover it"
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn run_script_hidden_on_non_unix() {
        let schemas = all_tool_schemas();
        let names = schema_names(&schemas);
        assert!(
            !names.contains(&"run_script"),
            "run_script requires Unix domain sockets — must not appear on other platforms"
        );
    }

    #[test]
    fn read_file_schema_exposes_only_line_range_contract() {
        let schemas = all_tool_schemas();
        let read_file = find_schema(&schemas, "read_file").expect("read_file schema must exist");
        let func = read_file
            .get("function")
            .expect("read_file schema must include function block");
        let params = func
            .get("parameters")
            .expect("read_file schema must include parameters");
        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "read_file should reject unknown top-level fields"
        );

        let properties = params
            .get("properties")
            .and_then(Value::as_object)
            .expect("read_file schema properties must be an object");
        for name in ["path", "start_line", "end_line", "outline"] {
            assert!(
                properties.contains_key(name),
                "read_file schema should expose `{name}`"
            );
        }
        for removed_arg in ["offset", "limit", "length", "count"] {
            assert!(
                !properties.contains_key(removed_arg),
                "read_file schema must not expose old/removed field `{removed_arg}`"
            );
        }
    }

    #[test]
    fn write_file_schema_requires_content_or_delete_contract() {
        let schemas = all_tool_schemas();
        let write_file = find_schema(&schemas, "write_file").expect("write_file schema must exist");
        let func = write_file
            .get("function")
            .expect("write_file schema must include function block");
        let params = func
            .get("parameters")
            .expect("write_file schema must include parameters");

        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "write_file should reject unknown top-level fields"
        );

        // Anthropic/Bedrock reject oneOf/allOf/anyOf at the top level of input_schema.
        // The write vs delete distinction is expressed through the typed
        // per-action extension rather than provider-rejected composition.
        assert!(
            params.get("oneOf").is_none(),
            "write_file parameters must not use top-level oneOf (Anthropic/Bedrock HTTP 400)"
        );
        assert!(
            params.get("allOf").is_none(),
            "write_file parameters must not use top-level allOf (Anthropic/Bedrock HTTP 400)"
        );
        assert!(
            params.get("anyOf").is_none(),
            "write_file parameters must not use top-level anyOf (Anthropic/Bedrock HTTP 400)"
        );

        // path must be the sole top-level required field.
        let required = params
            .get("required")
            .and_then(Value::as_array)
            .expect("write_file parameters must include a required array");
        assert!(
            required.iter().any(|v| v == "path"),
            "write_file must require path: {required:?}"
        );

        // Per-action required fields must be encoded in the vendor extension.
        let per_action = params.get("x-astra-per-action-required").expect(
            "write_file must use x-astra-per-action-required to encode per-mode requirements",
        );
        let write_req = per_action
            .get("write")
            .and_then(Value::as_array)
            .expect("x-astra-per-action-required must list fields required for write");
        assert!(
            write_req.iter().any(|v| v == "path") && write_req.iter().any(|v| v == "content"),
            "write action must require both path and content: {write_req:?}"
        );
        let delete_req = per_action
            .get("delete")
            .and_then(Value::as_array)
            .expect("x-astra-per-action-required must list fields required for delete");
        assert!(
            delete_req.iter().any(|v| v == "path"),
            "delete action must require path: {delete_req:?}"
        );
    }

    #[test]
    fn str_replace_schema_uses_provider_compatible_edit_mode_contract() {
        let schemas = all_tool_schemas();
        let str_replace =
            find_schema(&schemas, "str_replace").expect("str_replace schema must exist");
        let params = str_replace
            .pointer("/function/parameters")
            .expect("str_replace schema must include parameters");

        assert_eq!(
            params.get("additionalProperties").and_then(Value::as_bool),
            Some(false),
            "str_replace should reject unknown top-level fields"
        );
        assert!(
            params.get("oneOf").is_none()
                && params.get("allOf").is_none()
                && params.get("anyOf").is_none(),
            "str_replace parameters must avoid provider-rejected top-level schema composition"
        );

        assert!(
            params.get("required").is_none(),
            "str_replace cannot require top-level path because multi-file batch mode puts path inside edits[]"
        );

        let per_action = params.get("x-astra-per-action-required").expect(
            "str_replace must use x-astra-per-action-required to encode edit-mode requirements",
        );
        let single = per_action
            .get("single")
            .and_then(Value::as_array)
            .expect("single mode must be listed");
        assert!(
            ["path", "old_str", "new_str"]
                .iter()
                .all(|field| single.iter().any(|value| value.as_str() == Some(*field))),
            "single mode must require path, old_str, and new_str: {single:?}"
        );
        let batch_same_file = per_action
            .get("batch_same_file")
            .and_then(Value::as_array)
            .expect("same-file batch mode must be listed");
        assert!(
            ["path", "edits"].iter().all(|field| batch_same_file
                .iter()
                .any(|value| value.as_str() == Some(*field))),
            "same-file batch mode must require path and edits: {batch_same_file:?}"
        );

        let str_replace_description = str_replace
            .pointer("/function/description")
            .and_then(Value::as_str)
            .expect("str_replace schema must include a description");
        assert!(
            str_replace_description.contains("source-read opaque markers")
                && str_replace_description.contains("safe old_str anchors")
                && str_replace_description.contains("display-only")
                && str_replace_description.contains("foreign")
                && str_replace_description.contains("stale"),
            "str_replace must make the safe redacted-read edit contract discoverable: {str_replace_description}"
        );

        let read_file_description = find_schema(&schemas, "read_file")
            .and_then(|schema| schema.pointer("/function/description"))
            .and_then(Value::as_str)
            .expect("read_file schema must include a description");
        assert!(
            read_file_description.contains("source-read opaque markers")
                && read_file_description.contains("copied unchanged")
                && read_file_description.contains("never recover"),
            "read_file must describe how its opaque edit references flow to the editor: {read_file_description}"
        );
        let batch_multi_file = per_action
            .get("batch_multi_file")
            .and_then(Value::as_array)
            .expect("multi-file batch mode must be listed");
        assert!(
            ["edits[].path", "edits[].old_str", "edits[].new_str"]
                .iter()
                .all(|field| batch_multi_file
                    .iter()
                    .any(|value| value.as_str() == Some(*field))),
            "multi-file batch mode must require path inside each edit: {batch_multi_file:?}"
        );

        assert_eq!(
            params
                .pointer("/properties/edits/items/additionalProperties")
                .and_then(Value::as_bool),
            Some(false),
            "batch edit entries should reject unknown fields"
        );
        assert!(
            params
                .pointer("/properties/edits/items/properties/path")
                .is_some(),
            "batch edit entries should advertise optional per-edit path"
        );
    }

    fn find_schema<'a>(schemas: &'a [Value], name: &str) -> Option<&'a Value> {
        schemas.iter().find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some(name)
        })
    }

    #[test]
    fn shell_schema_timeout_defaults_are_structured() {
        let schemas = all_tool_schemas();
        let bash = find_schema(&schemas, "bash").expect("bash schema must exist");
        let ps = find_schema(&schemas, "powershell").expect("powershell schema must exist");
        let bash_description = bash
            .pointer("/function/description")
            .and_then(Value::as_str)
            .expect("bash schema must describe its execution contract");
        assert!(bash_description.contains("source_artifacts"));
        assert!(bash_description.contains("preserves them before spawn"));
        assert!(bash_description.contains("Checksum alone is not a backup"));
        assert_eq!(
            bash.pointer("/function/parameters/properties/source_artifacts/minItems")
                .and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(
            bash.pointer("/function/parameters/properties/source_artifacts/maxItems")
                .and_then(Value::as_u64),
            Some(crate::source_preimage::MAX_SOURCE_ARTIFACTS as u64)
        );
        assert!(
            bash.pointer("/function/parameters/properties/source_artifacts/description")
                .and_then(Value::as_str)
                .is_some_and(|description| description.contains("checksum alone is not a backup"))
        );
        assert_eq!(
            bash.pointer("/function/parameters/properties/timeout/default")
                .and_then(Value::as_f64),
            Some(crate::shell_ops::DEFAULT_BASH_TIMEOUT_SECS)
        );
        assert!(
            bash.pointer("/function/parameters/properties/timeout/description")
                .and_then(Value::as_str)
                .is_some_and(|description| {
                    description.contains("Outer execution timeout")
                        && description.contains("does not extend Astra's outer timeout")
                })
        );
        assert_eq!(
            ps.pointer("/function/parameters/properties/timeout/default")
                .and_then(Value::as_u64),
            Some(120)
        );
    }

    #[test]
    fn built_in_contract_enforces_per_action_required_fields() {
        let error = validate_tool_arguments(
            "memory",
            &json!({
                "action": "forget",
                "memory_id": "m1"
            }),
        )
        .unwrap_err();

        assert_eq!(error.action.as_deref(), Some("forget"));
        assert_eq!(
            error.issues,
            vec!["missing non-empty required field `reason`"]
        );
        assert_eq!(
            error.failure_evidence().kind,
            astra_core::ErrorKind::ToolInvalidArgs
        );
    }

    #[test]
    fn malformed_argument_sentinel_preserves_not_executed_receipt() {
        let error = validate_tool_arguments(
            "agent_fanout",
            &json!({
                "_parse_error": {
                    "kind": "invalid_json",
                    "category": "syntax",
                    "argument_bytes": 8699,
                    "column": 106,
                    "raw": "must not leak"
                }
            }),
        )
        .unwrap_err();
        let output: Value = serde_json::from_str(&error.output()).expect("typed failure JSON");
        assert_eq!(output["status"], "failed");
        assert_eq!(output["error_kind"], "tool_invalid_args");
        assert_eq!(output["advisory"]["executed"], false);
        assert_eq!(output["advisory"]["parse_error"]["column"], 106);
        assert!(output["advisory"]["parse_error"].get("raw").is_none());
    }

    #[test]
    fn built_in_contract_supports_bulk_identity_alternative() {
        validate_tool_arguments(
            "memory",
            &json!({
                "action": "forget",
                "memory_ids": ["m1", "m2"],
                "reason": "user selected these records"
            }),
        )
        .unwrap();

        let error = validate_tool_arguments(
            "memory",
            &json!({
                "action": "forget",
                "memory_ids": [],
                "reason": "user selected these records"
            }),
        )
        .unwrap_err();
        assert_eq!(
            error.issues,
            vec![
                "requires one of: memory_id or memory_ids or selection_id",
                "field `memory_ids` requires at least 1 item(s)",
            ]
        );
    }

    #[test]
    fn built_in_contract_validates_types_and_closed_objects() {
        let error = validate_tool_arguments(
            "reflect",
            &json!({
                "last_n": "many",
                "legacy_focus": "errors"
            }),
        )
        .unwrap_err();

        assert_eq!(
            error.issues,
            vec![
                "unknown field(s): legacy_focus",
                "field `last_n` has type string, expected \"integer\"",
            ]
        );
    }

    #[test]
    fn dynamic_tools_remain_owned_by_their_provider_contract() {
        validate_tool_arguments("mcp__custom__future_tool", &json!({"anything": true})).unwrap();
    }

    #[test]
    fn built_in_contract_recursively_validates_fanout_slots_and_bounds() {
        let error = validate_tool_arguments(
            "agent_fanout",
            &json!({
                "action": "start",
                "target_count": 0,
                "slots": [{"description": "review runtime"}]
            }),
        )
        .unwrap_err();

        assert_eq!(
            error.issues,
            vec![
                "field `slots` item 0 missing non-empty required field `prompt`",
                "field `target_count` must be at least 1",
            ]
        );
    }

    #[test]
    fn built_in_contract_validates_nested_any_of_variants() {
        validate_tool_arguments(
            "ask_user",
            &json!({"questions": [{"question": "Proceed?", "options": ["Yes", {"label": "No"}]}]}),
        )
        .unwrap();

        let error = validate_tool_arguments(
            "ask_user",
            &json!({"questions": [{"question": "Proceed?", "options": [{"description": "missing label"}]}]}),
        )
        .unwrap_err();
        assert_eq!(
            error.issues,
            vec![
                "field `questions` item 0 field `options` item 0 missing non-empty required field `label`"
            ]
        );
    }
}
