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

/// Structured failure returned when model-authored arguments do not satisfy
/// the invocation constraints encoded in the advertised built-in schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolArgumentValidationError {
    pub tool_name: String,
    pub action: Option<String>,
    pub issues: Vec<String>,
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
        crate::ToolResult::error(format!(
            "Error: {self}. Correct the arguments and issue one new call matching the advertised schema."
        ))
        .with_failure_evidence(evidence)
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
        Some(Value::Array(values)) => !values.is_empty(),
        Some(_) => true,
    }
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
        });
    };

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
                .all(|field| value_is_present(arguments.get(field)))
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

fn task_board_schema() -> Value {
    let mut subtask_props = serde_json::Map::new();
    subtask_props.insert(
        "id".to_string(),
        json!({"type": "string", "minLength": 1, "maxLength": crate::task_mgmt::MAX_SUBTASK_ID_CHARS}),
    );
    subtask_props.insert(
        "title".to_string(),
        json!({"type": "string", "minLength": 1, "maxLength": crate::task_mgmt::MAX_SUBTASK_TITLE_CHARS}),
    );
    subtask_props.insert(
        "description".to_string(),
        json!({"type": "string", "maxLength": crate::task_mgmt::MAX_SUBTASK_DESCRIPTION_CHARS}),
    );
    subtask_props.insert(
        "depends_on".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Sibling ids that must complete first."}),
    );
    subtask_props.insert(
        "owner".to_string(),
        json!({"type": "string", "minLength": 1, "maxLength": crate::task_mgmt::MAX_TASK_OWNER_CHARS}),
    );

    let mut props = serde_json::Map::new();
    props.insert(
        "action".to_string(),
        json!({"type": "string", "enum": crate::task_tool_contract::TASK_ACTIONS, "description": "Task-board operation; use only fields allowed for this action."}),
    );
    props.insert(
        "source_session_id".to_string(),
        json!({"type": "string", "description": "(adopt) Source session id."}),
    );
    props.insert(
        "older_than_days".to_string(),
        json!({"type": "integer", "description": "(archive bulk) Completed items older than N days."}),
    );
    props.insert(
        "user_status".to_string(),
        json!({"type": "string", "enum": ["active","pending","in_progress","paused","completed","failed","cancelled","archived","all"], "description": "(list_user) Default active = pending + in_progress + paused."}),
    );
    props.insert(
        "title".to_string(),
        json!({"type": "string", "minLength": 1, "maxLength": crate::task_mgmt::MAX_TASK_TITLE_CHARS, "description": "(create/update) Task title."}),
    );
    props.insert(
        "description".to_string(),
        json!({"type": ["string", "null"], "maxLength": crate::task_mgmt::MAX_TASK_DESCRIPTION_CHARS, "description": "(create/update) Definition of done; update may pass null to clear it."}),
    );
    props.insert(
        "task_id".to_string(),
        json!({"type": "string", "description": "(update/get/stop/adopt/archive) Task id."}),
    );
    props.insert(
        "new_status".to_string(),
        json!({"type": "string", "enum": ["pending","in_progress","paused","completed","failed","cancelled","deleted"], "description": "(update only; never with create) Parent/subtask outcome. Use completed only when the task's definition of done is satisfied; use failed when execution finished but the requested outcome was not achieved; paused when work is resumable. deleted keeps an audit tombstone."}),
    );
    props.insert(
        "status_filter".to_string(),
        json!({"type": "string", "enum": ["pending","in_progress","paused","completed","failed","cancelled","archived","deleted","all","active"], "description": "(list) Default active = pending + in_progress + paused. all includes tombstones."}),
    );
    props.insert(
        "subtask_id".to_string(),
        json!({"type": "string", "description": "(update) Subtask id."}),
    );
    props.insert(
        "active_form".to_string(),
        json!({"type": ["string", "null"], "minLength": 1, "maxLength": crate::task_mgmt::MAX_TASK_ACTIVE_FORM_CHARS, "description": "(create/update) Spinner text while in_progress; update may pass null to clear it."}),
    );
    props.insert(
        "owner".to_string(),
        json!({"type": ["string", "null"], "minLength": 1, "maxLength": crate::task_mgmt::MAX_TASK_OWNER_CHARS, "description": "(create/update) Owner; update may pass null to unassign."}),
    );
    props.insert(
        "metadata".to_string(),
        json!({"type": "object", "description": "(create/update) Key-value metadata; null deletes a key on update."}),
    );
    props.insert(
        "add_blocks".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "(create/update) Task ids this task blocks."}),
    );
    props.insert(
        "add_blocked_by".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "(create/update) Task ids blocking this task."}),
    );
    props.insert(
        "remove_blocks".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "(update) Remove blocks edges."}),
    );
    props.insert(
        "remove_blocked_by".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "(update) Remove blocked_by edges."}),
    );
    props.insert(
        "subtasks".to_string(),
        json!({
            "type": "array",
            "maxItems": crate::task_mgmt::MAX_CREATE_SUBTASKS,
            "description": "(create only) Optional subtasks; update existing subtasks with subtask_id + new_status.",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": Value::Object(subtask_props),
                "required": ["id", "title"]
            }
        }),
    );
    props.insert(
        "reason".to_string(),
        json!({"type": "string", "maxLength": crate::task_mgmt::MAX_TASK_STOP_REASON_CHARS, "description": "(update/stop/archive) Outcome evidence or subtask note. For terminal updates, state what was actually achieved or why it failed."}),
    );
    props.insert(
        "error_message".to_string(),
        json!({"type": "string", "maxLength": crate::task_mgmt::MAX_TASK_ERROR_MESSAGE_CHARS, "description": "(update) Failure/cancel reason."}),
    );

    let mut params = serde_json::Map::new();
    params.insert("type".to_string(), json!("object"));
    params.insert("additionalProperties".to_string(), json!(false));
    params.insert("properties".to_string(), Value::Object(props));
    params.insert("required".to_string(), json!(["action"]));
    params.insert(
        "x-astra-per-action-required".to_string(),
        json!({
            "create": ["title"],
            "update": ["task_id"],
            "get": ["task_id"],
            "stop": ["task_id"],
            "adopt": ["source_session_id", "task_id"]
        }),
    );
    params.insert(
        "x-astra-per-action-allowed".to_string(),
        crate::task_tool_contract::task_action_allowed_fields_json(),
    );

    json!({
        "type": "function",
        "function": {
            "name": crate::task_tool_contract::TASK_BOARD_TOOL_NAME,
            "description": "Durable task board. Pick one action; use only that action's allowed fields. create makes tasks; update changes status.",
            "parameters": Value::Object(params)
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

fn all_tool_schemas_core() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "publish_artifact",
                "description": "Publish a file that was already generated in the current session workspace or /tmp so the web UI can preview and download it. Use this after creating images, PDFs, CSVs, Markdown, HTML, or other files with bash/write_file/run_script. Do not use this to generate content directly; first create the file, then publish its path.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path of the generated file. Relative paths are resolved under the session workspace. Absolute paths are allowed only under the session workspace or /tmp."
                        },
                        "title": {"type": "string", "description": "Optional short display title for the artifact."},
                        "artifact_kind": {"type": "string", "description": "Optional stable kind such as image, pdf, markdown, html, data, text, code, archive, or file. If omitted, Astra infers it from the filename/content type."},
                        "content_type": {"type": "string", "description": "Optional MIME type. If omitted, Astra infers it from the file extension."},
                        "description": {"type": "string", "description": "Optional one-sentence explanation shown next to the artifact in the web UI."}
                    },
                    "required": ["path"]
                }
            }
        }),
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
                "description": "Execute a shell command. Use for builds, tests, installs, or actions with no dedicated tool. Identical commands are cached; set force=true to bypass.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "timeout": {"type": "number", "default": crate::shell_ops::DEFAULT_BASH_TIMEOUT_SECS, "description": "Timeout in seconds. Use a larger value for long builds/tests, e.g. cargo build or full test suites."},
                        "force": {"type": "boolean", "description": "Bypass the per-session identical-command cache."}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read file contents. Use exact fields only: path, start_line, end_line, outline. Line ranges are inclusive and 1-based. Omit end_line to read from start_line through the end of the file. For the first 50 lines: start_line=1, end_line=50. Set outline=true for function/class signatures only.",
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
                "description": "Targeted text replacement in files. Single mode: path+old_str+new_str. Batch mode: edits[]. Do not use aliases. For large changes (>4KB), use write_file.",
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
                "description": "List or restore file edits recorded by write_file and str_replace. Use scope=current_turn to undo this turn's recorded file edits, scope=file with path to restore the latest recorded edit for one file, scope=turn with turn_index to restore a previous turn, or scope=list to inspect available file edit rollback entries.",
                "parameters": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "scope": {"type": "string", "enum": ["current_turn","turn","file","list"], "description": "Rollback scope. Defaults to current_turn; path implies file scope."},
                        "path": {"type": "string", "description": "File path for scope=file."},
                        "turn_index": {"type": "integer", "description": "Turn index for scope=turn."},
                        "file_after_sequence": {"type": "integer", "description": "Only restore file edits recorded after this journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for file_after_sequence."}
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
                        "max_content": {"type": "integer", "description": "Max extracted content characters (default 80000)"},
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
                            "description": "Repository-relative file or directory path. Used by: diff, log, blame, checkout_file, contributors."
                        },
                        "file": {
                            "type": "string",
                            "description": "Repository-relative file path. Used by: file_history (required)."
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
                            "enum": ["remember","recall","session_audit","expand","forget","update","focus","reflect","profile","feedback"],
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
                        "importance": {"type": "number"},
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
                        "focus_type": {
                            "type": "string",
                            "enum": ["topic","tag","memory_id","session"],
                            "description": "focus target type."
                        },
                        "focus_value": {"type": "string", "description": "Focus target value."},
                        "boost": {"type": "number", "description": "Boost multiplier."},
                        "ttl_secs": {"type": "integer", "description": "Boost TTL seconds."},
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
                        "session_state_after_sequence": {"type": "integer", "description": "Only restore entries recorded after this rollback-journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for session_state_after_sequence."}
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
                        "database_after_sequence": {"type": "integer", "description": "Only restore database snapshot entries recorded after this journal sequence."},
                        "after_sequence": {"type": "integer", "description": "Alias for database_after_sequence."}
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "agent",
                "description": "Actions: spawn needs description+prompt (not task/type/agent_id; foreground fan-in by default; no background arg); get_result needs the returned agent_id of explicitly backgrounded work; run_chain needs name+description+steps.\n\n\
         Multi-agent operations. Actions: spawn, get_result, run_chain, send_message.\n\n\
         ## Required fields per action\n\
         - `spawn`: REQUIRES `action`, `description`, `prompt`. (Optional: `agent_type`, `model`, `max_turns`, `complexity`, `isolated`, `allowed_tools`, `name`.)\n\
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

         ## agent vs shell work vs task
         - `agent(spawn)` + optional `agent(get_result)`: one foreground sub-agent, or one explicitly backgrounded child the user later inspects.
         - `agent_fanout`: fixed-size parallel sub-agent groups with target-count accounting.
         - Shell commands/processes are separate execution tools; do not represent them as sub-agents.
         - `task_board`: session checklist / progress tracking — NOT an executor. Tasks track work; tools run it.",
                "parameters": {
                    "type": "object",
                    "x-astra-discovery-summary": "spawn: action+description+prompt; foreground fan-in unless the user backgrounds it. get_result: action+agent_id. run_chain: action+name+description+steps. send_message: action+to+message.",
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
                        "description": {"type": "string", "description": "Spawn UI summary or run_chain description."},
                        "prompt": {"type": "string", "description": "Full child task brief for spawn. Non-empty and required with description."},
                        "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"], "description": "Sub-agent persona (spawn). Default: general-purpose."},
                        "model": {"type": "string", "description": "Model override (spawn). Default: parent's model."},
                        "name": {"type": "string", "description": "Spawn mailbox label or required run_chain name."},
                        "input": {"type": "object", "description": "Optional run_chain template input."},
                        "rollback_on_failure": {"type": "boolean", "description": "Rollback bounded chain mutations after failure."},
                        "max_turns": {"type": "integer", "minimum": 1, "description": "Numeric child ceiling. When complexity is also present, the smaller of the numeric and complexity-derived ceilings wins."},
                        "complexity": {"type": "string", "enum": ["light","normal","deep"], "description": "Task-complexity ceiling: `light`≤10 turns, `normal`=agent default, `deep`=2× default. Prefer normal for scoped review/refactor work; use deep only when this child independently needs broad multi-step investigation. It never expands a smaller max_turns."},
                        "isolated": {"type": "boolean", "description": "Use isolated worktree (spawn)"},
                        "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "Tool allowlist (spawn)"},
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
                        "spawn": ["action", "description", "prompt", "agent_type", "model", "name", "max_turns", "complexity", "isolated", "allowed_tools"],
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
         Use this for independent parallel work. Put each full child instruction only in `slots[i].prompt`. Fanout already decomposes work: keep each slot narrowly scoped and normally use `normal` or an explicit bounded max_turns; do not mark every review slot `deep`. Use no brief/agents/background fields: never send top-level `brief`, `agents`, or `run_in_background`, and never put generated `agent_id` inside a slot. Start waits for accepted children concurrently and returns one canonical group result. In the terminal only the user may press Ctrl+B to hand the live group to the background; that explicit handoff returns stable child identities and later terminal results remain available through the group mailbox/get_results contract.",
                "parameters": {
                    "type": "object",
                    "x-astra-discovery-summary": "start: action+target_count+exactly target_count slots; each slot needs description+prompt. Shared config goes in defaults; no brief/agents/background. Results use group_id.",
                    "properties": {
                        "action": {"type": "string", "enum": ["start","get_results","stop_slot"]},
                        "group_id": {"type": "string", "description": "Fanout group id. Optional on start; required for get_results and stop_slot."},
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
                                    "description": {"type": "string", "description": "Short UI summary for this slot."},
                                    "prompt": {"type": "string", "description": "Full child task brief for this slot."},
                                    "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"]},
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
                                "agent_type": {"type": "string", "enum": ["explore","code-review","task","general-purpose"]},
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
                        "stop_slot": ["group_id", "slot_index"]
                    }
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "introspect",
                "description": "Read the live observation snapshot for the running turn/session. Use for self-checks: token/cache pressure, step latency/performance, tool health, recent rounds, runtime errors, stall/noise state, working memory, and plan/task/session lifecycle/resume state including the last lifecycle event when available. CLI/Edge can also inspect local cache and session_memory artifacts. For persisted multi-turn causal analysis, use reflect.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "topic": {"type": "string", "enum": ["overview","runtime","execution","knowledge"], "description": "Top-level observation area. Defaults to runtime; use execution for errors/trace and knowledge for session_memory/context artifacts."},
                        "facet": {"type": "string", "enum": ["session","overview","recent","errors","trace","volatile","stall","noise","cache","session_memory"], "description": "Specific live view. cache and session_memory require CLI/Edge-local artifacts; unavailable providers are reported in data_coverage."},
                        "depth": {"type": "string", "enum": ["hint","summary","diagnostic","forensic"], "description": "Output depth. hint is a compact nudge; diagnostic/forensic use the bounded full live renderer, including step latency/performance when available."},
                        "horizon": {"type": "string", "enum": ["now","current_turn","recent","turn","session","cross_session"], "description": "Time range label. Choose trace-like content with facet=trace, not by changing horizon."},
                        "source_policy": {"type": "string", "enum": ["auto","live_only","live_first","durable_first","local_only","cloud_only"], "description": "Preferred data source. Missing or unsatisfied providers are reported instead of fabricated."},
                        "include_context": {"type": "boolean", "description": "Request visible prompt/context facts when a provider is available; these are observed context, not durable truth."},
                        "format": {"type": "string", "enum": ["text","json"], "description": "Output format. text is default; json returns a structured read-only observation envelope."}
                    },
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "reflect",
                "description": "Analyze persisted observation evidence for the active session. Use for causal questions after errors, confusing tool choices, performance regressions, or trace review. Data may lag the current live turn; use introspect for immediate runtime health. Without an active session this returns reflect_requires_session.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "topic": {
                            "type": "string",
                            "enum": ["overview", "runtime", "execution", "knowledge"],
                            "description": "Top-level persisted evidence area. Use execution for errors/tools/trace, runtime for performance, knowledge for context/memory."
                        },
                        "facet": {
                            "type": "string",
                            "enum": ["overview", "performance", "errors", "tools", "trace", "context", "memory"],
                            "description": "Persisted evidence view under the selected topic. Examples: topic=execution facet=errors, topic=execution facet=trace, topic=runtime facet=performance."
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
                    "Search deferred tools. Keywords list candidates. `select:NAME[,NAME]` \
                     returns compact callable shape and queues schemas for the next request.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description":
                                "Keyword query, `select:NAME`, or `select:NAME1,NAME2`."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Keyword-mode result limit (default 5, max 20)."
                        }
                    },
                    "required": ["query"]
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
        // ── Durable task board ───────────────────────────────────────────
        task_board_schema(),
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

    #[test]
    fn agent_fanout_schema_exposes_atomic_group_contract() {
        let schemas = all_tool_schemas();
        let fanout = find_schema(&schemas, "agent_fanout").expect("agent_fanout schema must exist");
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
        assert!(
            slot_props.get("name").is_none(),
            "fanout slots should not expose spawn mailbox names as slot identity"
        );
    }

    #[test]
    fn agent_schema_structurally_owns_identity_fields_by_action() {
        let schemas = all_tool_schemas();
        let agent = find_schema(&schemas, "agent").expect("agent schema must exist");
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
    fn task_board_public_surface_is_single_action_resource_tool() {
        let schemas = all_tool_schemas();
        assert!(
            find_schema(&schemas, "task").is_none(),
            "model-facing task-board surface must not expose the old ambiguous task tool"
        );
        let task_board = find_schema(&schemas, "task_board").expect("task_board schema must exist");
        let params = &task_board["function"]["parameters"];
        assert_eq!(params["additionalProperties"], false);
        assert_eq!(required_fields(task_board), vec!["action".to_string()]);
        assert_eq!(
            params["properties"]["action"]["enum"]
                .as_array()
                .expect("task_board action enum")
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            crate::task_tool_contract::TASK_ACTIONS
        );
    }

    #[test]
    fn memory_and_task_board_schemas_stay_compact() {
        let schemas = all_tool_schemas();
        let memory = find_schema(&schemas, "memory").expect("memory schema must exist");
        let memory_tokens = schema_token_cost(memory);
        let task_board = find_schema(&schemas, "task_board").expect("task_board schema must exist");
        let task_board_tokens = schema_token_cost(task_board);

        assert!(
            memory_tokens <= 700,
            "memory schema regressed to {memory_tokens} tokens; keep it compact"
        );
        assert!(
            task_board_tokens <= 1100,
            "task_board schema regressed to {task_board_tokens} tokens; keep the resource tool compact"
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
            ("task_board", 140),
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
    fn task_board_schema_exposes_lifecycle_progress_and_dependencies() {
        let schemas = all_tool_schemas();
        let task_board = find_schema(&schemas, "task_board").expect("task_board schema");
        let properties = &task_board["function"]["parameters"]["properties"];

        for field in ["active_form", "add_blocks", "add_blocked_by", "subtasks"] {
            assert!(
                properties.get(field).is_some(),
                "task_board.create must expose {field}"
            );
        }
        for field in [
            "new_status",
            "subtask_id",
            "active_form",
            "add_blocks",
            "add_blocked_by",
            "remove_blocks",
            "remove_blocked_by",
            "error_message",
        ] {
            assert!(
                properties.get(field).is_some(),
                "task_board.update must expose {field}"
            );
        }

        assert_eq!(
            properties["subtasks"]["maxItems"].as_u64(),
            Some(crate::task_mgmt::MAX_CREATE_SUBTASKS as u64),
            "create schema should expose the same subtask fan-out limit as TaskManager"
        );
        assert_eq!(
            properties["subtasks"]["items"]["additionalProperties"], false,
            "subtask schema should reject unknown fields"
        );
        assert!(
            properties["subtasks"]["items"]["properties"]
                .get("owner")
                .is_some(),
            "subtask schema should expose the supported owner field"
        );
        assert!(
            properties["new_status"]["enum"]
                .as_array()
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("paused"))),
            "update schema should let the model intentionally pause/resume stale work"
        );
        for field in ["description", "active_form", "owner"] {
            assert_eq!(
                properties[field]["type"],
                json!(["string", "null"]),
                "update must be able to clear optional task field {field}"
            );
        }
        assert!(
            properties["status_filter"]["enum"]
                .as_array()
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("deleted"))),
            "list schema should let the model inspect deleted audit tombstones"
        );
        assert!(
            properties["user_status"]["enum"]
                .as_array()
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some("cancelled"))),
            "cross-session list schema should expose cancelled tasks"
        );
        let per_action_required =
            task_board["function"]["parameters"]["x-astra-per-action-required"]
                .as_object()
                .expect("task_board per-action required fields");
        assert_eq!(
            per_action_required["create"],
            json!(["title"]),
            "task_board.create required fields must stay explicit"
        );
        assert_eq!(
            per_action_required["adopt"],
            json!(["source_session_id", "task_id"]),
            "task_board.adopt required fields must stay explicit"
        );
        let per_action_allowed = task_board["function"]["parameters"]["x-astra-per-action-allowed"]
            .as_object()
            .expect("task_board per-action allowed fields");
        assert_eq!(
            per_action_allowed["create"],
            json!(crate::task_tool_contract::task_action_allowed_fields("create").unwrap()),
            "task_board.create allowed fields must be generated from task_tool_contract"
        );
        assert!(
            !per_action_allowed["create"]
                .as_array()
                .expect("create allowed fields")
                .iter()
                .any(|field| field.as_str() == Some("new_status")),
            "task_board.create must not advertise update-only status fields"
        );
        assert_eq!(
            per_action_allowed["update"],
            json!(crate::task_tool_contract::task_action_allowed_fields("update").unwrap()),
            "task_board.update allowed fields must be generated from task_tool_contract"
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
        let properties = introspect["function"]["parameters"]["properties"]
            .as_object()
            .expect("introspect parameters properties must be an object");
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
            "source_policy",
            "include_context",
            "format",
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
        assert_eq!(
            bash.pointer("/function/parameters/properties/timeout/default")
                .and_then(Value::as_f64),
            Some(crate::shell_ops::DEFAULT_BASH_TIMEOUT_SECS)
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
    fn built_in_contract_enforces_action_owned_fields() {
        validate_tool_arguments(
            "task_board",
            &json!({"action": "create", "title": "Implement canonical boundary"}),
        )
        .unwrap();

        let error = validate_tool_arguments(
            "task_board",
            &json!({
                "action": "create",
                "title": "Implement canonical boundary",
                "new_status": "completed"
            }),
        )
        .unwrap_err();
        assert_eq!(
            error.issues,
            vec!["field(s) not allowed for action `create`: new_status"]
        );

        let blank_owner = validate_tool_arguments(
            "task_board",
            &json!({"action": "create", "title": "Task", "owner": "   "}),
        )
        .unwrap_err();
        assert_eq!(
            blank_owner.issues,
            vec!["field `owner` requires at least 1 character(s)"]
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
