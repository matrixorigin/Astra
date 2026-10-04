//! Tool schema definitions for all edge tools.
//
//! Each schema is a JSON object following the OpenAI function-calling format:
//! `{ "type": "function", "function": { "name": ..., "description": ..., "parameters": ... } }`

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::OnceLock;

use serde_json::{Map, Value, json};

pub const PER_ACTION_REQUIRED_KEY: &str = "x-astra-per-action-required";
pub const PER_ACTION_ANY_OF_REQUIRED_KEY: &str = "x-astra-per-action-any-of-required";
pub const PER_ACTION_ALLOWED_KEY: &str = "x-astra-per-action-allowed";
pub const ACTION_SURFACES_KEY: &str = "x-astra-action-surfaces";
pub const SURFACE_DESCRIPTIONS_KEY: &str = "x-astra-surface-descriptions";
pub const SURFACE_DISCOVERY_SUMMARIES_KEY: &str = "x-astra-surface-discovery-summaries";
/// Producer-owned compact discovery text for each action. Consumers may
/// project this map after a typed action subset is selected; they never need
/// to parse the full function description to discover which action remains.
pub const PER_ACTION_DISCOVERY_SUMMARIES_KEY: &str = "x-astra-per-action-discovery-summaries";

/// Rebuild the compact discovery summary after a typed action projection.
///
/// The retained action order is taken from the executable schema, while the
/// wording comes only from producer-owned structured metadata. If a producer
/// has not supplied the map, the existing summary is intentionally preserved.
pub fn project_action_discovery_summary(
    parameters: &mut Map<String, Value>,
    retained_actions: &[String],
) {
    let Some(summaries) = parameters
        .get(PER_ACTION_DISCOVERY_SUMMARIES_KEY)
        .and_then(Value::as_object)
    else {
        return;
    };
    let projected = retained_actions
        .iter()
        .filter_map(|action| {
            summaries
                .get(action)
                .and_then(Value::as_str)
                .map(|summary| format!("{action}: {summary}"))
        })
        .collect::<Vec<_>>();
    if projected.is_empty() {
        parameters.remove("x-astra-discovery-summary");
    } else {
        parameters.insert(
            "x-astra-discovery-summary".to_string(),
            Value::String(projected.join(". ")),
        );
    }
}

/// Render the producer-declared conditional argument contract in a compact,
/// provider-neutral form.
///
/// The internal `x-astra-*` annotations are useful to Astra's validator and
/// provider adapters, but a deferred `tool_search` result deliberately strips
/// those annotations to keep the activation payload small. Keeping this
/// projection in the schema owner means discovery and provider wire adapters
/// expose the same required/allowed fields without asking consumers to infer
/// them from tool names or prose.
#[must_use]
pub fn action_contract_description(parameters: &Map<String, Value>) -> Option<String> {
    let per_action_required = parameters
        .get(PER_ACTION_REQUIRED_KEY)
        .and_then(Value::as_object);
    let per_action_any_of = parameters
        .get(PER_ACTION_ANY_OF_REQUIRED_KEY)
        .and_then(Value::as_object);
    let per_action_allowed = parameters
        .get(PER_ACTION_ALLOWED_KEY)
        .and_then(Value::as_object);

    let mut actions = BTreeSet::new();
    for source in [per_action_required, per_action_any_of, per_action_allowed]
        .into_iter()
        .flatten()
    {
        actions.extend(source.keys().map(String::as_str));
    }

    let mut requirements = Vec::new();
    for action in actions {
        if let Some(fields) = per_action_required
            .and_then(|source| source.get(action))
            .and_then(Value::as_array)
        {
            let fields = fields.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if !fields.is_empty() {
                requirements.push(format!("{action} requires {}", fields.join(" + ")));
            }
        }

        if let Some(alternatives) = per_action_any_of
            .and_then(|source| source.get(action))
            .and_then(Value::as_array)
        {
            let alternatives = alternatives
                .iter()
                .filter_map(Value::as_array)
                .map(|fields| {
                    fields
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" + ")
                })
                .filter(|fields| !fields.is_empty())
                .collect::<Vec<_>>();
            if !alternatives.is_empty() {
                requirements.push(format!(
                    "{action} also requires one of {}",
                    alternatives.join(" or ")
                ));
            }
        }

        if let Some(fields) = per_action_allowed
            .and_then(|source| source.get(action))
            .and_then(Value::as_array)
        {
            let fields = fields.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if !fields.is_empty() {
                requirements.push(format!("{action} accepts only {}", fields.join(" + ")));
            }
        }
    }

    (!requirements.is_empty()).then(|| format!("Action contract: {}.", requirements.join("; ")))
}

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
    for keyword in ["anyOf", "oneOf"] {
        let Some(alternatives) = schema.get(keyword).and_then(Value::as_array) else {
            continue;
        };
        let mut matches = 0;
        let mut best_failure: Option<Vec<String>> = None;
        let has_type_compatible_branch = alternatives.iter().any(|candidate| {
            candidate
                .get("type")
                .is_none_or(|expected| schema_type_matches(value, expected))
        });
        for candidate in alternatives {
            if has_type_compatible_branch
                && candidate
                    .get("type")
                    .is_some_and(|expected| !schema_type_matches(value, expected))
            {
                continue;
            }
            let mut candidate_issues = Vec::new();
            validate_schema_value(value, candidate, path, true, &mut candidate_issues);
            if candidate_issues.is_empty() {
                matches += 1;
                if keyword == "anyOf" {
                    break;
                }
            } else if best_failure
                .as_ref()
                .is_none_or(|best| candidate_issues.len() < best.len())
            {
                best_failure = Some(candidate_issues);
            }
        }
        if matches == 0 {
            issues.extend(
                best_failure
                    .unwrap_or_else(|| vec![format!("{path} has no matching {keyword} branch")]),
            );
            return;
        }
        if keyword == "oneOf" && matches != 1 {
            issues.push(format!("{path} must match exactly one advertised branch"));
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
    if let Some(expected) = schema.get("const")
        && value != expected
    {
        issues.push(format!("{path} differs from its advertised constant"));
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
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true)
            && values
                .iter()
                .enumerate()
                .any(|(index, value)| values[..index].contains(value))
        {
            issues.push(format!("{path} requires unique items"));
        }
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
    validate_tool_arguments_against_schema(tool_name, args, schema)
}

/// Validate an invocation against the exact provider-owned function schema.
///
/// Builtins use [`validate_tool_arguments`], while dynamic provider tools must
/// use this same validator before approval or dispatch. The schema is supplied
/// by the authenticated provider contract; unknown names are never accepted
/// merely because they are absent from Astra's builtin registry.
pub fn validate_tool_arguments_against_schema(
    tool_name: &str,
    args: &Value,
    schema: &Value,
) -> Result<(), ToolArgumentValidationError> {
    let Some(parameter_schema @ Value::Object(parameters)) = schema
        .get("function")
        .and_then(|function| function.get("parameters"))
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

    validate_schema_value(args, parameter_schema, "", false, &mut issues);

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

pub fn submit_task_resolution_schema() -> Value {
    json!({
        "type": "function", "function": {
            "name": "submit_task_resolution",
            "description": "Submit an evidence-linked model assessment only when the runtime requests reconciliation. Name exact failed and later supporting call IDs for the same verification target; retain unknowns and remaining gaps. Keep verification_target within 256 characters, rationale within 1024 characters, and each remaining gap within 256 characters. For a supported conclusion, remaining_gaps must be empty. Submission is not verification success and never replaces required checks.",
            "parameters": {
                "type": "object", "additionalProperties": false,
                "required": ["verification_target", "failed_call_ids", "evidence_call_ids", "conclusion", "rationale", "remaining_gaps"],
                "properties": {
                    "verification_target": {"type": "string", "maxLength": 256},
                    "failed_call_ids": {"type": "array", "minItems": 1, "maxItems": 32, "items": {"type": "string", "maxLength": 256}},
                    "evidence_call_ids": {"type": "array", "maxItems": 32, "items": {"type": "string", "maxLength": 256}},
                    "conclusion": {"type": "string", "enum": ["supported", "partial", "unknown"]},
                    "rationale": {"type": "string", "maxLength": 1024},
                    "remaining_gaps": {"type": "array", "maxItems": 32, "items": {"type": "string", "maxLength": 256}}
                }
            }
        }
    })
}

fn start_work_schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "start_work",
            "description": "Establish the conversation's one canonical Work with an initial ordered task list. This is the genesis transition: call it only when no Work is bound; once bound, never call start_work again. When the user explicitly requests durable Work with 2+ independently useful deliverables/evidence tracks, call this before exploration. Count user acceptance units: A and B are separate only when each owes its own payload or evidence and remains useful alone; inputs serving one combined conclusion are one outcome. Same-turn multi-agent topology alone uses independent agent.spawn calls, or agent_fanout when group control is needed, not Work; simple questions and one-shot responses do not use Work. activation=start assigns the first task; use activation=defer when this turn only establishes/prepares a plan or explicitly says not to execute; defer creates no attempt. Declare every known outcome initially, including dependent outcomes; task identities are server-owned; declare only explicit execution prerequisites via after_initial_tasks. Omit only outcomes to be decided, discovered, added, replaced, or cancelled later until the typed graph-update boundary. One bounded operation producing all requested evidence is one task; exclude synthesis, formatting, reporting, and restatement. Preserve N explicitly named execution tracks as exactly N tasks unless scope changes. A successful result normally includes initial_task; execute it directly instead of calling run_next_work_item. For a bound Work, inspect_work_plan then propose_work_plan is the only graph-change path.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "x-astra-discovery-summary": "Declare known outcomes, including dependencies. Omit later decisions/additions/replacements until graph update. Preserve exactly N named initial tracks. start assigns the first task; use its returned assignment.",
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
                        "description": "Known acceptance units, including dependent outcomes; honor counts. Keep observation/verification/report together; omit later decisions/additions/replacements.",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "objective": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "expected_result": {"type": "string", "minLength": 1, "maxLength": 8192},
                                "after_initial_tasks": {
                                    "type": "array", "maxItems": 8, "uniqueItems": true,
                                    "items": {"type": "integer", "minimum": 1, "maximum": 8},
                                    "description": "Explicit prerequisites only: 1-based initial task indices that must deliver first. Omit for independent tasks."
                                }
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
                                "declaration_state": {"type": "string", "enum": ["active", "superseded", "cancelled"], "description": "Enum is active|superseded|cancelled (not cancel). Use active only when the same semantic item continues; retire the old identity with superseded or cancelled before a fresh replacement addition."}
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
            "description": "Execute a PowerShell command. Use for Windows shell tasks, pwsh scripts, and cross-platform automation when PowerShell syntax is preferred over bash. PREFER dedicated tools (glob, grep, read_file, write_file, str_replace) over shell commands when they cover the operation.",
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
        let retained_actions = parameters
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get("action"))
            .and_then(Value::as_object)
            .and_then(|action| action.get("enum"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        project_action_discovery_summary(parameters, &retained_actions);
        for key in [
            PER_ACTION_REQUIRED_KEY,
            PER_ACTION_ANY_OF_REQUIRED_KEY,
            PER_ACTION_ALLOWED_KEY,
            PER_ACTION_DISCOVERY_SUMMARIES_KEY,
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

fn delegation_agent_type_schema() -> Value {
    let selection = "With an admitted directory, use its exact profile ID. Otherwise omitted/explore/code-review are read-only (no shell); choose task/general-purpose for shell or mutation, within parent permissions.";
    json!({
        "type": "string",
        "minLength": 1,
        "description": selection,
        "x-astra-discovery-summary": "Exact directory ID; else explore=no shell(default), task requests shell.",
    })
}

fn requested_model_policy_schema() -> Value {
    json!({
        "description": "Requested model behavior, distinct from the resolved Offering. For a model name coming from the human request, omit this field—including exact names and harmless spelling variants—so one candidate-aware admission can resolve it against the authorized catalog. Use a fixed selector only when the caller already has an exact authorized Offering ID; never put a display name in offering_id, normalize a human name into a fixed selector, or guess an Offering ID. Unresolved or unavailable requirements block new child execution. Explicit inherit cannot override a hard user requirement. Auto cost-priority and balanced requests are preserved, but currently fail closed before any child starts because comparable task-level cost, quality, and completion-time evidence is unavailable.",
        "oneOf": [
            {
                "type": "object",
                "properties": {"mode": {"const": "inherit"}},
                "required": ["mode"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "mode": {"const": "fixed"},
                    "selector": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "offering_id"},
                                    "offering_id": {"type": "string", "minLength": 1, "maxLength": 64}
                                },
                                "required": ["kind", "offering_id"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "kind": {"const": "configured_name"},
                                    "model_name": {"type": "string", "minLength": 1, "maxLength": 256},
                                    "source": {"type": "string", "minLength": 1, "maxLength": 128}
                                },
                                "required": ["kind", "model_name"],
                                "additionalProperties": false
                            }
                        ]
                    }
                },
                "required": ["mode", "selector"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "mode": {"const": "auto"},
                    "strategy": {"type": "string", "enum": ["cost_priority", "balanced"]}
                },
                "required": ["mode", "strategy"],
                "additionalProperties": false
            }
        ]
    })
}

fn fanout_reasoning_schema() -> Value {
    json!({
        "description": "Exact reasoning control. Omit to inherit parent thinking for the same Offering; model_default explicitly uses the target default. Different Offerings never inherit parent controls.",
        "type": "object",
        "required": ["mode"],
        "oneOf": [
            {"properties": {"mode": {"enum": ["model_default", "on", "off"]}}, "additionalProperties": false},
            {"properties": {"mode": {"const": "enabled"}, "budget_tokens": {"type": "integer", "minimum": 1024, "maximum": 4294967295_u64}}, "required": ["budget_tokens"], "additionalProperties": false},
            {"properties": {"mode": {"const": "adaptive"}, "effort": {"type": "string", "enum": ["low", "medium", "high", "max"]}}, "required": ["effort"], "additionalProperties": false}
        ]
    })
}

fn all_tool_schemas_core() -> Vec<Value> {
    heap_schema_vec![
        submit_task_resolution_schema(),
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
            "description": "Workspace root is default; workdir selects a bounded call directory. source_artifacts preserves them before spawn. Checksum alone is not a backup; no process-persistence guarantee.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "workdir": {"type": "string", "minLength": 1, "description": "Optional execution directory for this call only. Relative paths resolve from the workspace root; absolute paths must remain inside it. The directory must already exist. Defaults to the workspace root and does not persist. Executors without pinned-subdirectory support reject subdirectories rather than weaken path confinement."},
                        "mode": {"type": "string", "enum": ["verify"], "description": "Optional explicit workspace verification contract. Omit this field for calculations, read-only probes, diagnostics, tests, and commands whose stdout is the evidence. Use it only for a foreground command that verifies a bound workspace stayed unchanged after edits; it succeeds only when the command exits zero and the executor proves that fact, and it must not be used for commands that write files."},
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
                            "description": "Required when this command may change state outside the bound workspace. List the smallest absolute external roots whose state must change. Astra captures bounded pre/post fingerprints and issues completion evidence only for an observed delta under authoritative process ownership. Paths inside or overlapping the workspace, relative/traversal paths, unobservable roots, background tasks, and unchanged state fail closed. Omit this only for workspace-confined work or an explicit read-only verification; do not use it for workspace files."
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
                "name": "worktree",
                "description": "Enter or exit a session-scoped Git worktree. Enter changes this session's working workspace; exit restores it. Use bash for ordinary git commands.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["enter", "exit"]},
                        "branch": {"type": "string", "description": "New branch name; required for enter."},
                        "exit_action": {"type": "string", "enum": ["keep", "remove"], "description": "Keep or remove the worktree on exit; defaults to keep."},
                        "discard_changes": {"type": "boolean", "description": "Allow discarding changes when exiting with remove; defaults to false."}
                    },
                    "required": ["action"],
                    "additionalProperties": false
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
                            "enum": ["all","session"],
                            "description": "Recall scope: all is owner-scoped; session is strict current-session isolation."
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
                        "value": {"type": "number", "description": "Numeric config value. Count paths require an integer; compression threshold accepts a fractional number."},
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
                "description": "Actions: spawn needs description+prompt (not task/type/agent_id; returns a launched receipt promptly so the parent continues; no background arg); list reads child status; get_result needs the returned agent_id; run_chain needs name+description+steps.\n\n\
         Multi-agent and local fixed-chain operations. Actions: spawn, list, get_result, wait, run_chain, send_message. wait observes runtime activity without cancelling children on timeout; get_result inspects an outcome, not a polling loop. `run_chain` is a local executor pipeline, not a durable task list. If the user asks for task/Work tracking and `start_work` is visible, call `start_work` directly instead of using `agent`.\n\n\
         ## Required fields per action\n\
         - `spawn`: REQUIRES `action`, `description`, `prompt`. (Optional: `agent_type`, `requested_model_policy`, `reasoning`, `initial_turns`, `max_output_tokens`, `complexity`, `isolated`, `allowed_tools`, `name`, `inherit_prefix`.)\n\
         - `list`: REQUIRES `action`; optional exact `agent_id`. Read-only status of this agent's direct owned children in the current session's in-memory cache. No database query, terminal wait, or result collection. Missing entries are unknown, not completed.\n\
         - `wait`: REQUIRES `action`; optional `timeout_ms` (1..300000, default 30000). Yield until current-run semantic input or timeout. Sibling tools finish first. Timeout does not cancel children. Do not poll `get_result` to wait.\n\
         - `get_result`: REQUIRES `action`, `agent_id`. Collect the child outcome when needed, including after an ordinary spawn. May briefly wait or reconcile durable state; use `list` for status only and do not busy-poll.\n\
         - `run_chain`: REQUIRES `action`, `name`, `description`, `steps`.\n\
         - `send_message`: REQUIRES `action`, `to`, `message`; `message_type=answer` also requires the exact `request_id` shown on the incoming question. A child asking its parent uses `to=parent` and `message_type=question`, not `ask_user` (which addresses the human user). The parent answers with `message_type=answer` and that exact request ID. Returns `queued` when the routing/transport path accepts the message. Receiver observation does not prove model inclusion, compliance, or task completion.\n\n\
         For `spawn`, pass both non-empty fields: `description` (short UI summary) and `prompt` (full child brief). Do NOT pass a top-level `task` field. Do NOT pass `type`; use `agent_type`. Do NOT pass `inherit_context`. `agent_id` is for `list` and `get_result`; never prefill it on `spawn`. Astra generates that runtime id for you. Status filters and result calls must reuse the exact returned `agent_id`. If you need a mailbox label, use `name`, but `name` is not valid for `list` or `get_result`.\n\n\
         Model choice uses `requested_model_policy`, not a `model` field. If the `agent` tool is already visible, call it directly; do not call `tool_search` or `model_catalog` first just to spawn one child. For any model name coming from the user—including natural-language variations in spelling, spacing, or component order—omit `requested_model_policy`; one candidate-aware admission resolves it against the authorized catalog. Use a fixed selector only when an exact authorized Offering ID is already supplied by the caller. Never guess an Offering ID, inspect configuration, or turn a human name into a fixed selector. When an admitted profile directory is present, `agent_type` must be its exact non-empty directory/profile ID; do not omit it or substitute a builtin persona. Omit `agent_type` only when no admitted directory is present; the trusted runtime then supplies the bounded default.\n\n\
         ## Spawn example\n\
         `{\"action\":\"spawn\",\"description\":\"Audit auth flow\",\"prompt\":\"Read src/auth/* and report token-handling bugs. Return numbered findings.\"}`\n\n\
         ## Execution mode\n\
         `spawn` returns a `launched` receipt with a runtime-generated `agent_id` promptly after execution ownership is established, while the child runs and the parent continues independent work. When no relevant independent work remains, propose a final answer: the runtime waits and presents the child outcome before accepting it. Do not use shell sleep or busy-poll status to wait. No background flag or Ctrl+B is needed. The receipt proves launch, not completion; collect the child outcome before relying on it. Normal model admission, tool permissions, execution deadlines, lineage, and cancellation ownership still apply. Launching does not extend the deadline or grant permissions.\n\n\
         ## Parallel sub-agent fan-out\n\
         For independent parallel tasks, call `agent` with `action=spawn` once per child. Each launch has its own receipt and may succeed or fail independently; report partial outcomes honestly. Do not issue the same child task twice unless the user explicitly asks for independent duplicate runs. Use `agent_fanout` only when the user needs all-child preflight, target-count accounting, or group-wide control. Preflight does not guarantee every child will execute successfully. Do not simulate a group with an `agents:[...]` payload on `agent`. `agent_fanout.start` launches the admitted slots concurrently and returns a launch receipt; the parent-owned completion boundary prevents finalization before terminal child outcomes are staged. Slots may include `id` as a caller-facing label; runtime-generated `agent_id` values come back in the result.\n\
         For plan lifecycle, if `enter_plan_mode` / `exit_plan_mode` are visible in the current tool surface, call them directly; never wrap them in the `agent` `run_chain` action.\n\
         Do NOT pass an `agents:[...]` payload, do NOT pass a top-level `task` field, and do NOT wrap spawn arguments under a `spawn` field. `agent` launches one child; `agent_fanout` launches a fixed parallel group.

         ## Canonical Work and delegation
         - `agent(spawn)`: one concurrent, parent-owned child; `agent(list)` observes status and `agent(get_result)` collects its outcome when needed.
         - `agent_fanout`: fixed-size parallel sub-agent groups with target-count accounting.
         - Shell commands/processes are separate execution tools; do not represent them as sub-agents.
         - When no canonical Work exists and the current turn requires durable task tracking, establish it with `start_work` before delegating. When canonical Work already exists, keep that Work as the durable scope rather than trying to create another one. `agent` and `agent_fanout` do not themselves create or replace a canonical task list.
         - `start_work` may return `initial_task`, and `settle_work_item` may return `next_task`. Each is already the server-selected primary-session assignment: execute it directly. Call `run_next_work_item({})` only when neither response supplied an assignment. Treat an assigned task's expected result as its stop boundary: gather sufficient direct evidence, settle immediately when satisfied, and do not expand into adjacent investigation. Generic `agent` and `agent_fanout` are reserved for real isolation or parallelism boundaries; a WorkItem alone is not a delegation reason.
         - Background task tools only observe or control execution; they are not a planning system.",
                "parameters": {
                    "type": "object",
                    "x-astra-action-surfaces": {
                        "spawn": ["local", "server"],
                        "list": ["local", "server"],
                        "get_result": ["local", "server"],
                        "wait": ["local", "server"],
                        "run_chain": ["local"],
                        "send_message": ["local", "server"]
                    },
                    "x-astra-surface-descriptions": {
                        "server": "Server-owned single-agent lifecycle. If visible, call it directly; do not call tool_search or model_catalog first just to spawn. Actions: spawn, list, get_result, send_message. When an admitted profile directory is present, use its exact non-empty directory/profile ID for agent_type; do not omit it or substitute a builtin persona. Without a directory, omit agent_type for the bounded read-only default; choose a builtin persona only then when mutation or the full surface is required. Spawn needs description+prompt and returns a launch receipt, not completion; execution deadlines, tool permissions, lineage, and cancellation still apply. list is read-only status of this agent's direct owned children; get_result collects an outcome; wait observes runtime activity instead of polling. The parent-owned completion boundary waits and presents the child result. A child asks its parent with message_type=question, not ask_user, and the parent answers with the exact request_id. For a user model name, omit requested_model_policy for one catalog admission; fixed selectors require an exact authorized Offering ID. Never inspect workspace files, model configuration, or credentials or normalize a human name into a selector. Use visible start_work for durable Work."
                    },
                    "x-astra-surface-discovery-summaries": {
                        "server": "requested_model_policy: user model=omit (no catalog prerequisite); fixed=Offering ID; no config reads; hard reqs bind; spawn=launched; propose final; runtime waits; no shell sleep; agent question."
                    },
                    "x-astra-per-action-discovery-summaries": {
                        "spawn": "requested_model_policy: user model=omit (no catalog prerequisite); fixed=Offering ID; no config reads; hard reqs bind; spawn=launched; propose final; runtime waits; no shell sleep; agent question.",
                        "get_result": "action+returned agent_id; collect outcome when needed; may briefly wait or reconcile durable state; use list for status; do not busy-poll",
                        "list": "action; optional exact agent_id; read-only in-memory status of direct owned children in this session; no database query, terminal wait, or result collection; absent means unknown",
                        "run_chain": "local fixed pipeline with action+name+description+steps; never a durable task list",
                        "send_message": "action+to+message; child asks parent via to=parent, message_type=question (not ask_user); parent answers with the exact request_id"
                    },
                    "x-astra-discovery-summary": "requested_model_policy: user model=omit (no catalog prerequisite); fixed=Offering ID; no config reads; hard reqs bind; spawn=launched; propose final; runtime waits; no shell sleep; agent question.",
                    "properties": {
                        "action": {"type": "string", "enum": ["spawn","list","get_result","wait","run_chain","send_message"]},
                        "timeout_ms": {"type":"integer", "minimum":1, "maximum":300000, "description":"Observation wait timeout (wait). Default 30000 ms. Does not cancel children."},
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
                        "prompt": {"type": "string", "description": "Full self-contained child task brief for spawn. Include the constraints, conditional mappings, and expected output needed to finish; only a choice the child must ask about may be left unresolved. Non-empty and required with description."},
                        "agent_type": delegation_agent_type_schema(),
                        "requested_model_policy": requested_model_policy_schema(),
                        "reasoning": fanout_reasoning_schema(),
                        "name": {"type": "string", "description": "Action label when accepted by the selected action."},
                        "input": {"type": "object", "description": "Optional run_chain template input."},
                        "rollback_on_failure": {"type": "boolean", "description": "Rollback bounded chain mutations after failure."},
                        "initial_turns": {"type": "integer", "minimum": 1, "description": "Optional first execution slice; renewable while progress continues. This is not a hard limit. When complexity is also present, the smaller initial slice wins."},
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
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Initial-slice hint: `light`≤10 turns, `normal`=agent default, `deep`=2× default. Prefer normal for scoped review/refactor work; use deep only when this child independently needs broad multi-step investigation. It never expands a smaller initial_turns hint."},
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
                        "agent_id": {"type": "string", "description": "For get_result (required) or list (optional): exact runtime-generated agent_id, not a spawn name. Never prefill this on spawn."},
                        "to": {"type": "string", "description": "REQUIRED for action='send_message'. Active child/peer agent_id, related exact run_id within the current delegation boundary, 'parent', or '*' for broadcast."},
                        "message": {"description": "REQUIRED for action='send_message'. Concise coordination message (at most 3000 characters); share an artifact for larger content."},
                        "message_type": {"type": "string", "enum": ["text","question","answer","instruction","progress","result","shutdown_request","shutdown_response"]},
                        "request_id": {"type": "string", "description": "Exact incoming question ID; required with message_type=answer. Optional correlation id for other follow-ups."}
                    },
                    "required": ["action"],
                    "additionalProperties": false,
                    "x-astra-per-action-required": {
                        "spawn": ["description", "prompt"],
                        "run_chain": ["name", "description", "steps"],
                        "get_result": ["agent_id"],
                        "list": [],
                        "wait": [],
                        "send_message": ["to", "message"]
                    },
                    "x-astra-per-action-allowed": {
                        "spawn": ["action", "description", "prompt", "agent_type", "requested_model_policy", "reasoning", "name", "initial_turns", "max_output_tokens", "complexity", "isolated", "allowed_tools", "inherit_prefix", "work_item"],
                        "get_result": ["action", "agent_id"],
                        "list": ["action", "agent_id"],
                        "wait": ["action", "timeout_ms"],
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
         - `get_results`: requires `action` and returned `group_id`. It takes a short non-blocking snapshot; the parent-owned completion boundary independently stages terminal child outcomes, so do not busy-poll. Use optional `slot_index`, `offset`, and `max_bytes` for one bounded result window; `results[].next_call` gives the next window.\n\
         - `stop_slot`: requires `action`, `group_id`, and `slot_index`; it stops one running child.\n\n\
         - `stop_group`: requires `action` and `group_id`; it requests cancellation for every non-terminal child in one group operation.\n\n\
         Use this for independent parallel work only when the user request or loaded workflow explicitly requires parallelism. Put one concise child brief in each slot. An omitted model policy uses the admitted profile's model default, then the parent Offering; explicit inherit selects the parent Offering. Exact authorized Offering and reasoning overrides require atomic admission before any slot starts. For any model name coming from the user, omit requested_model_policy and let one candidate-aware admission resolve it against the authorized catalog; use a fixed selector only when an exact authorized Offering ID is already supplied. Never inspect workspace configuration or credentials or normalize a human name into a selector. Only tools exposed in a child's own tool surface are usable; do not start workspace-dependent slots while the workspace provider is unavailable. When an admitted profile directory is present, set `agent_type` on each slot or in `defaults` to the exact non-empty profile/directory ID from that directory; do not omit it or substitute explore, code-review, task, or general-purpose. Without a directory, omit `agent_type` for the bounded read-only default, or choose a builtin persona only when mutation or the full surface is required. Never paste file contents or prior tool output into a slot prompt. Use `allowed_tools`, not `tools`; do not send `brief`, `agents`, `background`, or generated `agent_id` fields. Start launches admitted slots concurrently and returns a receipt; use get_results for a bounded snapshot, never busy-poll.",
                "parameters": {
                    "type": "object",
                    "x-astra-per-action-discovery-summaries": {
                        "start": "requested_model_policy: user model=omit (no catalog prerequisite); fixed=Offering ID; no config reads; hard requirements bind; start: target_count slots, description+prompt; atomic.",
                        "get_results": "action+group_id; use bounded result windows and follow next_call",
                        "stop_slot": "action+group_id+slot_index",
                        "stop_group": "action+group_id"
                    },
                     "x-astra-discovery-summary": "requested_model_policy: user model=omit (no catalog prerequisite); fixed=Offering ID; no config reads; hard requirements bind; start: target_count slots, description+prompt; atomic.",
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
                                    "agent_type": delegation_agent_type_schema(),
                                    "initial_turns": {"type": "integer", "minimum": 1, "description": "Renewable first execution slice, not a hard limit."},
                                    "max_output_tokens": {"type": "integer", "minimum": 1},
                                    "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                    "isolated": {"type": "boolean"},
                                    "allowed_tools": {"type": "array", "items": {"type": "string"}},
                                    "requested_model_policy": requested_model_policy_schema(),
                                    "reasoning": fanout_reasoning_schema()
                                },
                                "required": ["description", "prompt"]
                            }
                        },
                        "defaults": {
                            "type": "object",
                            "description": "Shared runtime configuration inherited by every slot. Slot-level overrides take precedence.",
                            "additionalProperties": false,
                            "properties": {
                                "agent_type": delegation_agent_type_schema(),
                                "initial_turns": {"type": "integer", "minimum": 1, "description": "Renewable first execution slice, not a hard limit."},
                                "max_output_tokens": {"type": "integer", "minimum": 1},
                                "complexity": {"type": "string", "enum": ["light","normal","deep"]},
                                "isolated": {"type": "boolean"},
                                "allowed_tools": {"type": "array", "items": {"type": "string"}},
                                "requested_model_policy": requested_model_policy_schema(),
                                "reasoning": fanout_reasoning_schema()
                            }
                        },
                        "slot_index": {"type": "integer", "minimum": 0, "description": "REQUIRED for stop_slot. Optional for get_results to read one slot result window."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Optional for get_results with slot_index. Byte offset for the slot result window. Default 0."},
                        "max_bytes": {"type": "integer", "minimum": 4, "maximum": 65536, "description": "Optional for get_results. Maximum UTF-8 result bytes per slot window. Default 8192; valid range 4-65536."}
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
                "name": "model_catalog",
                "description": "Read the current user's authorized active Chat models. Returns one complete JSON page with exact offering/name/provider/access identities, capabilities, nullable prices, revision and continuation cursor. Call only when the user asks which delegated models are available, compares models, or leaves model identity genuinely ambiguous; concrete user-named models, including harmless separator/case variants, are resolved during admission, so never call this merely to spawn one. Never inspect workspace configuration for model identity. Omit cursor and catalog_revision on the first page; send both unchanged when following next_cursor.",
                "parameters": {
                    "type": "object",
                    "x-astra-discovery-summary": "Authorized Chat model availability/comparison only; never a user-named spawn prerequisite. JSON only. limit defaults 16; follow next_cursor with catalog_revision. No workspace config reads. Discovery is not execution admission.",
                    "properties": {
                        "limit": {"type": "integer", "minimum": 1, "maximum": 32, "default": 16},
                        "cursor": {"type": "string", "minLength": 1, "maxLength": 2048},
                        "catalog_revision": {"type": "string", "minLength": 71, "maxLength": 71}
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Read bounded current runtime observations or an identified artifact; use model_catalog for available Chat models. Current live queries default to facet=overview and depth=summary; use depth=hint for quick checks and diagnostic detail only for a concrete gap or requested audit. Exact historical execution evidence comes from Server Explain: explain={target:previous} excludes the current root and selects the latest eligible prior root, which is not guaranteed to be the previous user turn; target=run requires run_id and identifies that exact run. A run that requested Explain returns its selected artifact handle; an ordinary run returns a bounded durable projection without creating an artifact. CLI/Edge Explain selectors are unsupported. For session-level prior causality, use reflect; do not use an ordinary live query as historical evidence or combine it with history unless current state is also requested or a concrete evidence gap requires one. Do not repeat identical diagnostics without new state or a requested deep audit; a cached repeat adds no evidence. Live horizons are not historical truth. Observation/evidence URNs such as urn:astra:observation:* are citations, not artifact handles. Use artifact://session/tool-result/<opaque_token> for artifacts; omit it for live state.",
                "parameters": {
                    "type": "object",
                    "x-astra-discovery-summary": "Live runtime observation and Server Explain/artifact recovery. For authorized Chat models use model_catalog, not workspace configuration.",
                    "properties": {
                        "topic": {"type": "string", "enum": ["overview","runtime","execution","knowledge"], "description": "Area: runtime is default; execution covers errors/trace, knowledge covers context artifacts."},
                        "facet": {"type": "string", "enum": ["session","overview","recent","errors","trace","volatile","stall","noise","cache","session_memory"], "description": "Live runtime observation facet."},
                        "depth": {"type": "string", "enum": ["hint","summary","diagnostic","forensic"], "description": "Default/hint/summary project bounded execution facts. diagnostic/forensic include full live data or raw Explain artifact pages when available."},
                        "horizon": {"type": "string", "enum": ["now","current_turn","recent","turn","session","cross_session"], "description": "Window label for live observation. Historical labels return a marked recent live projection, not exact execution history; use Explain with an exact run or reflect for persisted history."},
                        "question": {"type": "string", "description": "Optional context label; it does not widen evidence."},
                        "source_policy": {"type": "string", "enum": ["auto","live_only","live_first","durable_first","local_only","cloud_only"], "description": "Source preference; unavailable coverage is reported."},
                        "include_context": {"type": "boolean", "description": "Include available observed prompt/context facts."},
                        "format": {"type": "string", "enum": ["text","json"], "description": "Output format; default text."},
                        "artifact": {"type": "string", "description": "Opaque tool-result or Explain handle; paginate with offset/max_bytes. Never a file path."},
                        "explain": {
                            "type": "object",
                            "properties": {
                                "target": {"type": "string", "enum": ["previous", "run"]},
                                "run_id": {"type": "string", "minLength": 1}
                            },
                            "required": ["target"],
                            "additionalProperties": false,
                            "description": "Server only. run requires run_id; previous forbids it. Exclusive with artifact; offset must be 0. target=run is exact active-session execution evidence; target=previous selects the latest eligible prior root without older fallback."
                        },
                        "offset": {"type": "integer", "minimum": 0, "description": "Artifact byte offset; default 0."},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": 65536, "description": "Artifact page bytes or bounded projection output budget. Default Explain discovery returns a summary with a detail handle; projections retain whole records and omission counts without pagination. A larger budget does not recover upstream capture omissions."}
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "reflect",
                "description": "Analyze persisted, session-level observation evidence for the active session. Use for causal questions across prior turns after errors, confusing tool choices, performance regressions, or trace review. For a retrospective, make one composite topic=overview facet=overview call with the concrete question; it combines decisions, tools, errors, trace and provider coverage, so do not fan out facets unless it reports a gap. Reflect has no exact run/turn selector: do not label session aggregates as facts from one specific execution unless the cited evidence carries that identity. A live introspect call is not required; use it only for a requested current-state check or a concrete evidence gap. Without an active session this returns reflect_requires_session.",
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
                    "Select contracts (`select:NAME[,NAME]`). Reuse via invoke_tool. Do not select \
                     tools already in tools[]; when `agent` is visible, call its resident spawn \
                     shape directly. Selection does not change tools[] or permission.",
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

    #[test]
    fn action_discovery_summary_projects_only_retained_typed_actions() {
        let mut parameters = serde_json::Map::from_iter([
            (
                PER_ACTION_DISCOVERY_SUMMARIES_KEY.to_string(),
                json!({
                    "start": "target_count+slots",
                    "get_results": "group_id+bounded window",
                    "stop_group": "group_id"
                }),
            ),
            (
                "x-astra-discovery-summary".to_string(),
                json!("start: target_count+slots"),
            ),
        ]);
        project_action_discovery_summary(
            &mut parameters,
            &["get_results".to_string(), "stop_group".to_string()],
        );
        assert_eq!(
            parameters["x-astra-discovery-summary"],
            "get_results: group_id+bounded window. stop_group: group_id"
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

    // ── agent tool: parent-owned launch and observation contract ───────────

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
            "single-child concurrency needs no model flag; explicit handoff remains a user control"
        );
        assert!(props.get("background").is_none());
        assert!(props.get("max_turns").is_none());
        assert!(props.get("initial_turns").is_some());
        assert_eq!(props["agent_type"]["type"], "string");
        assert_eq!(props["agent_type"]["minLength"], 1);
        assert!(props["agent_type"].get("enum").is_none());
    }

    #[test]
    fn agent_model_policy_describes_canonical_selection_and_human_name_handling() {
        let schemas = all_tool_schemas();
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
        let description = agent["function"]["description"]
            .as_str()
            .expect("agent description");
        assert!(description.contains("`requested_model_policy`, `reasoning`"));
        assert!(description.contains("Never guess an Offering ID, inspect configuration"));
        assert!(!description.contains("Optional: `agent_type`, `model`"));
        let properties = &agent["function"]["parameters"]["properties"];
        assert!(properties.get("model").is_none());
        let policy_description = properties["requested_model_policy"]["description"]
            .as_str()
            .expect("requested model policy description");
        assert!(policy_description.contains("For a model name coming from the human request"));
        assert!(policy_description.contains(
            "Use a fixed selector only when the caller already has an exact authorized Offering ID"
        ));
        assert!(policy_description.contains("never put a display name in offering_id"));
        assert!(policy_description.contains("authorized catalog"));
        assert!(policy_description.contains("omit this field"));
        assert!(policy_description.contains("cannot override a hard user requirement"));
        assert!(description.contains("exact non-empty directory/profile ID"));
        assert!(description.contains("do not omit it or substitute a builtin persona"));
    }

    #[test]
    fn deferred_delegation_discovery_preserves_user_model_requirements() {
        for surface in ["server", "local"] {
            let mut schemas = all_tool_schemas();
            project_action_schemas_for_surface(&mut schemas, surface);

            for name in ["agent", "agent_fanout"] {
                let schema = find_schema(&schemas, name).expect("delegation schema must exist");
                let selection = crate::tool_search::tool_selection_contract(schema)
                    .expect("delegation schema must have a discovery contract");
                if surface == "server" {
                    assert_eq!(
                        selection["description_truncated"], false,
                        "load-bearing Server guidance must fit deferred discovery for {name}"
                    );
                }
                let summary = selection["description"]
                    .as_str()
                    .expect("delegation discovery summary");
                if name == "agent" {
                    assert!(
                        summary.contains("requested_model_policy"),
                        "{surface}: {summary}"
                    );
                    assert!(summary.contains("user model"), "{surface}: {summary}");
                    assert!(
                        summary.contains("hard requirements bind")
                            || summary.contains("hard reqs bind"),
                        "{surface}: {summary}"
                    );
                    assert!(summary.contains("no config reads"), "{surface}: {summary}");
                    assert!(summary.contains("launched"), "{surface}: {summary}");
                    assert!(
                        summary.contains("propose final") || summary.contains("proposes final"),
                        "{surface}: {summary}"
                    );
                    assert!(summary.contains("agent question"), "{surface}: {summary}");
                    assert!(summary.contains("shell sleep"), "{surface}: {summary}");
                    assert!(!summary.contains("foreground"), "{surface}: {summary}");
                } else {
                    let lower = summary.to_ascii_lowercase();
                    assert!(lower.contains("user model"), "{surface}/{name}: {summary}");
                    assert!(
                        lower.contains("omit") && lower.contains("catalog"),
                        "{surface}/{name}: {summary}"
                    );
                    assert!(
                        lower.contains("hard requirements bind"),
                        "{surface}/{name}: {summary}"
                    );
                    assert!(
                        summary.contains("no config reads"),
                        "{surface}/{name}: {summary}"
                    );
                }
            }
        }
    }

    #[test]
    fn agent_manifest_summary_keeps_coordination_cues_within_its_budget() {
        for surface in ["local", "server"] {
            let mut schemas = all_tool_schemas();
            project_action_schemas_for_surface(&mut schemas, surface);
            let agent = find_schema(&schemas, "agent").unwrap();
            let summary = agent["function"]["parameters"]["x-astra-discovery-summary"]
                .as_str()
                .unwrap();
            let visible: String = summary.chars().take(180).collect();
            for cue in ["requested_model_policy", "runtime waits", "no shell sleep"] {
                assert!(visible.contains(cue), "{surface}: missing {cue}: {visible}");
            }
            if surface == "server" {
                assert!(summary.chars().count() <= 180, "{surface}: {summary}");
                assert!(visible.contains("agent question"), "{surface}: {visible}");
            }
        }
    }

    #[test]
    fn agent_observation_contract_survives_surface_and_action_projection() {
        for surface in ["local", "server"] {
            let mut schemas = all_tool_schemas();
            project_action_schemas_for_surface(&mut schemas, surface);
            let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
            let description = agent["function"]["description"].as_str().unwrap();
            assert!(description.contains("direct owned children"));
            assert!(description.contains("execution deadlines"));
            assert!(description.contains("tool permissions"));
            assert!(description.contains("parent-owned completion boundary"));
            assert!(description.contains("message_type=question"));
            assert!(description.contains("ask_user"));
            if surface == "server" {
                assert!(description.contains("one catalog admission"));
                assert!(description.contains(
                    "Never inspect workspace files, model configuration, or credentials"
                ));
            }
            let params = &agent["function"]["parameters"];
            let child_brief = params["properties"]["prompt"]["description"]
                .as_str()
                .expect("spawn prompt description");
            assert!(child_brief.contains("self-contained"));
            assert!(child_brief.contains("conditional mappings"));
            assert!(
                params["properties"]["action"]["enum"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("list"))
            );
            assert_eq!(params[PER_ACTION_REQUIRED_KEY]["list"], json!([]));
            assert_eq!(
                params[PER_ACTION_ALLOWED_KEY]["list"],
                json!(["action", "agent_id"])
            );
            if surface == "server" {
                let selection = crate::tool_search::tool_selection_contract(agent).unwrap();
                assert!(
                    selection["description"]
                        .as_str()
                        .unwrap()
                        .contains("no shell sleep")
                );
                assert!(
                    selection["description"]
                        .as_str()
                        .unwrap()
                        .contains("runtime waits")
                );
                let mut spawn = agent.clone();
                project_action_discovery_summary(
                    spawn["function"]["parameters"].as_object_mut().unwrap(),
                    &["spawn".to_string()],
                );
                let selection = crate::tool_search::tool_selection_contract(&spawn).unwrap();
                assert!(
                    selection["description"]
                        .as_str()
                        .unwrap()
                        .contains("no shell sleep")
                );
                let mut message = agent.clone();
                project_action_discovery_summary(
                    message["function"]["parameters"].as_object_mut().unwrap(),
                    &["send_message".to_string()],
                );
                let selection = crate::tool_search::tool_selection_contract(&message).unwrap();
                assert_eq!(selection["description_truncated"], false);
                assert!(
                    selection["description"]
                        .as_str()
                        .is_some_and(|text| text.contains("message_type=question")
                            && text.contains("not ask_user"))
                );
            }

            for (action, guidance) in [
                (
                    "list",
                    "read-only in-memory status of direct owned children",
                ),
                ("get_result", "may briefly wait or reconcile durable state"),
            ] {
                let mut selected = agent.clone();
                project_action_discovery_summary(
                    selected["function"]["parameters"].as_object_mut().unwrap(),
                    &[action.to_string()],
                );
                let selection = crate::tool_search::tool_selection_contract(&selected).unwrap();
                assert_eq!(selection["description_truncated"], false);
                let summary = selection["description"].as_str().unwrap();
                assert!(summary.contains(guidance), "{surface}/{action}: {summary}");
                assert!(!summary.contains("spawn:"), "{surface}/{action}: {summary}");
            }
        }
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
            description.contains("user request or loaded workflow explicitly requires parallelism")
        );
        let params = &fanout["function"]["parameters"];
        assert!(
            params["properties"]["slots"]["items"]["properties"]
                .get("max_turns")
                .is_none()
        );
        assert!(
            params["properties"]["slots"]["items"]["properties"]
                .get("initial_turns")
                .is_some()
        );

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
        assert_eq!(params["properties"]["max_bytes"]["minimum"], 4);
        assert_eq!(
            params["properties"]["slots"]["items"]["required"],
            json!(["description", "prompt"])
        );
        assert!(params["properties"].get("run_in_background").is_none());
        let slot_props = &params["properties"]["slots"]["items"]["properties"];
        for agent_type in [
            &slot_props["agent_type"],
            &params["properties"]["defaults"]["properties"]["agent_type"],
        ] {
            assert_eq!(agent_type["type"], "string");
            assert_eq!(agent_type["minLength"], 1);
            assert!(agent_type.get("enum").is_none());
        }
        assert!(
            slot_props.get("id").is_some(),
            "fanout slots must expose the canonical caller-facing identity field"
        );
        assert!(slot_props.get("slot_id").is_none());
        assert_eq!(
            slot_props["requested_model_policy"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            slot_props["reasoning"]["oneOf"].as_array().unwrap().len(),
            3
        );
        assert_eq!(
            params["properties"]["defaults"]["properties"]["requested_model_policy"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(params["properties"]["defaults"]["properties"]["reasoning"].is_object());
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
            description
                .to_ascii_lowercase()
                .contains("only tools exposed in a child's own tool surface are usable")
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
        assert!(description.contains("exact non-empty profile/directory ID"));
        assert!(description.contains(
            "do not omit it or substitute explore, code-review, task, or general-purpose"
        ));
    }

    #[test]
    fn admitted_team_profile_ids_validate_through_native_and_deferred_schemas() {
        let team =
            astra_services::team_persistence::builtin_teams("schema-owner", "2026-10-03T00:00:00Z")
                .into_iter()
                .next()
                .expect("builtin team fixture");
        let profile_id =
            astra_services::team_persistence::resolve_member_to_profile(&team.members[0], &team)
                .agent_id;

        let agent_args = json!({
            "action": "spawn",
            "description": "Review the runtime",
            "prompt": "Return evidence from the runtime review.",
            "agent_type": profile_id.clone(),
        });
        let fanout_args = json!({
            "action": "start",
            "target_count": 1,
            "slots": [{
                "description": "Review the runtime",
                "prompt": "Return evidence from the runtime review.",
                "agent_type": profile_id.clone(),
            }],
            "defaults": {"agent_type": profile_id.clone()},
        });

        validate_tool_arguments("agent", &agent_args)
            .expect("native agent schema accepts the admitted profile ID");
        validate_tool_arguments("agent_fanout", &fanout_args)
            .expect("native fanout schema accepts the admitted profile ID");

        for surface in ["local", "server"] {
            let mut schemas = all_tool_schemas();
            project_action_schemas_for_surface(&mut schemas, surface);

            for (name, args) in [("agent", &agent_args), ("agent_fanout", &fanout_args)] {
                let schema = find_schema(&schemas, name).expect("delegation schema");
                validate_tool_arguments_against_schema(name, args, schema).unwrap_or_else(|err| {
                    panic!("{surface} deferred schema rejected admitted profile ID: {err}")
                });
            }
        }

        let mut blank_agent = agent_args.clone();
        blank_agent["agent_type"] = json!(" ");
        assert!(validate_tool_arguments("agent", &blank_agent).is_err());

        let mut blank_fanout = fanout_args.clone();
        blank_fanout["slots"][0]["agent_type"] = json!("");
        assert!(validate_tool_arguments("agent_fanout", &blank_fanout).is_err());
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

        validate_tool_arguments("agent", &json!({"action": "list"}))
            .expect("list needs only its action");
        validate_tool_arguments(
            "agent",
            &json!({"action": "list", "agent_id": "runtime-id"}),
        )
        .expect("list accepts an exact child identity filter");
        for field in ["name", "prompt", "run_in_background"] {
            let invalid = json!({"action": "list", field: "not-a-list-argument"});
            assert!(
                validate_tool_arguments("agent", &invalid).is_err(),
                "list must reject {field}"
            );
        }
        validate_tool_arguments("agent", &json!({"action": "get_result"}))
            .expect_err("get_result still requires the returned child identity");
    }

    #[test]
    fn start_work_schema_exposes_a_small_server_owned_task_list() {
        let schemas = all_tool_schemas();
        let schema = find_schema(&schemas, "start_work").expect("start_work schema");
        let serialized_bytes = serde_json::to_vec(schema)
            .expect("start_work schema must serialize deterministically")
            .len();
        assert!(
            serialized_bytes <= 3_500,
            "start_work must keep its complete always-load wire schema compact; got {serialized_bytes} bytes"
        );
        let description = schema["function"]["description"]
            .as_str()
            .expect("start_work description");
        assert!(description.contains("initial_task"));
        assert!(description.contains("Count user acceptance units"));
        assert!(description.contains("each owes its own payload or evidence"));
        assert!(description.contains("execute it directly"));
        assert!(description.contains("genesis transition"));
        assert!(description.contains("never call start_work again"));
        assert!(description.contains("propose_work_plan"));
        assert!(description.contains("task identities are server-owned"));
        assert!(description.contains("explicit execution prerequisites via after_initial_tasks"));
        assert!(
            description
                .contains("Declare every known outcome initially, including dependent outcomes")
        );
        assert_eq!(
            required_fields(schema),
            vec![
                "goal".to_string(),
                "activation".to_string(),
                "tasks".to_string()
            ]
        );
        let parameters = &schema["function"]["parameters"];
        let selection = crate::tool_search::tool_selection_contract(schema)
            .expect("start_work must have a discovery contract");
        assert_eq!(
            selection["description_truncated"], false,
            "load-bearing Work chronology must survive deferred discovery"
        );
        assert!(selection["description"].as_str().is_some_and(|summary| {
            summary.contains("Declare known outcomes, including dependencies")
                && summary.contains("Omit later decisions/additions/replacements")
                && summary.contains("exactly N named initial tracks")
        }));
        let task_properties = parameters["properties"]["tasks"]["items"]["properties"]
            .as_object()
            .expect("task fields must be structurally declared");
        assert_eq!(
            task_properties
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "after_initial_tasks",
                "expected_result",
                "objective"
            ]),
            "tasks expose user precedence, but never server-owned identity or kind"
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
                        "expected_result": "A concise conclusion",
                        "after_initial_tasks": [1]
                    }
                ]
            }),
        )
        .expect("explicit prerequisites are part of the initial Work contract");
        for prerequisites in [json!([0]), json!([9]), json!([1, 1]), json!(["task-1"])] {
            assert!(
                validate_tool_arguments(
                    "start_work",
                    &json!({
                        "goal": "Verify evidence", "activation": "start",
                        "tasks": [{"objective": "Verify", "expected_result": "Evidence",
                            "after_initial_tasks": prerequisites}]
                    })
                )
                .is_err()
            );
        }
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
    fn array_uniqueness_is_enforced_only_when_declared() {
        for unique in [false, true] {
            let schema = json!({"function": {"parameters": {
                "type": "object", "properties": {"values": {
                    "type": "array", "uniqueItems": unique
                }}
            }}});
            for values in [
                json!([1, 1]),
                json!([{"a": 1}, {"a": 1}]),
                json!([[1], [1]]),
            ] {
                assert_eq!(
                    validate_tool_arguments_against_schema(
                        "external_tool",
                        &json!({"values": values}),
                        &schema
                    )
                    .is_err(),
                    unique
                );
            }
            for values in [json!([]), json!([1, 2]), json!([{"a": 1}, {"a": 2}])] {
                validate_tool_arguments_against_schema(
                    "external_tool",
                    &json!({"values": values}),
                    &schema,
                )
                .expect("distinct values");
            }
        }
    }

    #[test]
    fn start_work_schema_is_identical_across_local_and_server_projection() {
        let schemas = all_tool_schemas();
        let canonical = find_schema(&schemas, "start_work")
            .expect("start_work schema")
            .clone();

        for surface in ["local", "server"] {
            let mut projected = vec![canonical.clone()];
            project_action_schemas_for_surface(&mut projected, surface);
            assert_eq!(
                projected[0], canonical,
                "start_work has no surface-specific action union; {surface} projection must preserve its complete wire contract"
            );
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
        assert!(description.contains("Live horizons are not historical truth"));
        assert!(description.contains("use reflect"));
        assert!(description.contains("depth=summary"));
        assert!(
            description.contains("Exact historical execution evidence comes from Server Explain")
        );
        assert!(description.contains("target=run requires run_id and identifies that exact run"));
        assert!(description.contains("diagnostic detail only for a concrete gap"));
        assert!(description.contains("requested deep audit"));
        assert!(description.contains("cached repeat adds no evidence"));
        assert!(description.contains("not guaranteed to be the previous user turn"));
        assert!(description.contains("urn:astra:observation:*"));
        assert!(description.contains("artifact://session/tool-result/<opaque_token>"));
        let properties = introspect["function"]["parameters"]["properties"]
            .as_object()
            .expect("introspect parameters properties must be an object");
        assert!(
            properties["horizon"]["description"]
                .as_str()
                .expect("introspect horizon description")
                .contains("marked recent live projection")
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
            "explain",
            "offset",
            "max_bytes",
        ] {
            assert!(
                properties.contains_key(key),
                "introspect schema should expose normalized observation parameter `{key}`"
            );
        }
        assert_eq!(
            properties.len(),
            12,
            "introspect prose compression must not add or remove parameters"
        );
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
        assert_eq!(
            enum_values(&properties["explain"]["properties"]["target"]),
            vec!["previous", "run"]
        );
        assert_eq!(properties["explain"]["additionalProperties"], false);
        assert!(description.contains("current root"));
        assert_eq!(properties["max_bytes"]["maximum"], 65_536);
        assert_eq!(properties["max_bytes"]["minimum"], 1);
        assert_eq!(properties["offset"]["minimum"], 0);
        assert_eq!(enum_values(&properties["format"]), vec!["text", "json"]);
    }

    #[test]
    fn reflect_schema_defines_session_scope_without_live_pairing() {
        let schemas = all_tool_schemas();
        let reflect = find_schema(&schemas, "reflect").expect("reflect schema must exist");
        let description = reflect["function"]["description"]
            .as_str()
            .expect("reflect description must be present");
        assert!(description.contains("session-level"));
        assert!(description.contains("no exact run/turn selector"));
        assert!(description.contains("A live introspect call is not required"));
        assert!(!description.contains("Pair this persisted view with introspect"));
        let properties = reflect["function"]["parameters"]["properties"]
            .as_object()
            .expect("reflect properties must be an object");
        assert!(!properties.contains_key("run_id"));
        assert!(!properties.contains_key("turn_id"));
    }

    #[test]
    fn model_catalog_has_a_small_strict_discovery_contract() {
        let schemas = all_tool_schemas();
        let catalog = find_schema(&schemas, "model_catalog").expect("model catalog tool exists");
        let params = &catalog["function"]["parameters"];
        assert_eq!(params["additionalProperties"], false);
        let properties = params["properties"].as_object().unwrap();
        assert_eq!(properties.len(), 3);
        assert_eq!(properties["limit"]["minimum"], 1);
        assert_eq!(properties["limit"]["maximum"], 32);
        assert!(properties.contains_key("cursor"));
        assert!(properties.contains_key("catalog_revision"));
        assert!(properties.get("facet").is_none());
        assert!(properties.get("format").is_none());
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
    fn session_config_schema_accepts_numeric_values_and_rejects_other_types() {
        for (path, value) in [
            ("compression.compression_threshold", json!(0.81)),
            ("memory.retrieval_top_k", json!(6)),
        ] {
            validate_tool_arguments(
                "session",
                &json!({"action":"config", "path":path, "value":value}),
            )
            .unwrap();
        }
        for value in [json!("6"), json!(true), Value::Null, json!({}), json!([])] {
            assert!(
                validate_tool_arguments(
                    "session",
                    &json!({"action":"config", "path":"memory.retrieval_top_k", "value":value})
                )
                .is_err()
            );
        }
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
        assert_eq!(
            bash.pointer("/function/parameters/properties/workdir/type")
                .and_then(Value::as_str),
            Some("string")
        );
        assert!(
            bash.pointer("/function/parameters/properties/mode/description")
                .and_then(Value::as_str)
                .is_some_and(|description| {
                    description.contains("Omit this field for calculations")
                        && description.contains("after edits")
                })
        );
        validate_tool_arguments(
            "bash",
            &json!({"command": "pwd", "workdir": "crates/runtime"}),
        )
        .expect("bash must accept a call-scoped workdir");
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
    fn child_reasoning_validates_discriminators_and_closed_variants() {
        let spawn = |reasoning| {
            json!({
                "action":"spawn", "description":"Check", "prompt":"Return the result", "reasoning":reasoning
            })
        };
        for reasoning in [
            json!({"mode":"model_default"}),
            json!({"mode":"on"}),
            json!({"mode":"off"}),
            json!({"mode":"enabled","budget_tokens":1024}),
            json!({"mode":"adaptive","effort":"high"}),
        ] {
            validate_tool_arguments("agent", &spawn(reasoning)).expect("valid reasoning variant");
        }
        for reasoning in [
            json!({}),
            json!({"mode":"invented"}),
            json!({"mode":"adaptive"}),
            json!({"mode":"adaptive","effort":"invented"}),
            json!({"mode":"adaptive","effort":"high","budget_tokens":1024}),
            json!({"mode":"enabled"}),
            json!({"mode":"enabled","budget_tokens":1023}),
            json!({"mode":"enabled","budget_tokens":4294967296_u64}),
            json!({"mode":"on","effort":"high"}),
            json!("on"),
        ] {
            assert!(
                validate_tool_arguments("agent", &spawn(reasoning.clone())).is_err(),
                "invalid reasoning accepted: {reasoning}"
            );
        }
        let schema = json!({"function":{"parameters":{"type":"object","properties":{
            "value":{"oneOf":[{"type":"integer"},{"type":"number"}]}
        }}}});
        assert!(
            validate_tool_arguments_against_schema("dynamic", &json!({"value":1}), &schema)
                .is_err(),
            "overlapping oneOf branches are not an exclusive match"
        );
    }

    #[test]
    fn root_union_branches_enforce_their_required_fields() {
        for keyword in ["oneOf", "anyOf"] {
            let mut parameters = json!({"type":"object"});
            parameters[keyword] = json!([{"required":["a"]},{"required":["b"]}]);
            let schema = json!({"function":{"parameters":parameters}});
            for arguments in [json!({"a":1}), json!({"b":1})] {
                validate_tool_arguments_against_schema("dynamic", &arguments, &schema).unwrap();
            }
            assert!(
                validate_tool_arguments_against_schema("dynamic", &json!({}), &schema).is_err()
            );
            assert_eq!(
                validate_tool_arguments_against_schema("dynamic", &json!({"a":1,"b":1}), &schema)
                    .is_ok(),
                keyword == "anyOf"
            );
        }
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
