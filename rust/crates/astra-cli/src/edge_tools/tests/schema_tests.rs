use super::*;

// ── all_tool_schemas ──────────────────────────────────────────────────────

#[test]
fn all_tool_schemas_non_empty() {
    let schemas = all_tool_schemas();
    assert!(!schemas.is_empty(), "should have at least one tool schema");
}

#[test]
fn all_tool_schemas_have_function_name() {
    for schema in all_tool_schemas() {
        let name = schema
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str());
        assert!(name.is_some(), "schema missing function.name: {schema}");
        assert!(!name.unwrap().is_empty());
    }
}

#[test]
fn all_tool_schemas_have_description() {
    for schema in all_tool_schemas() {
        let desc = schema
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str());
        assert!(
            desc.is_some(),
            "schema missing description: {:?}",
            schema["function"]["name"]
        );
    }
}

#[test]
fn tool_schemas_include_core_tools() {
    let names: Vec<String> = all_tool_schemas()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    // Consolidated tools: git, github, memory, session, mo, agent cover
    // the legacy individual tools (git_status, github_ci_status, etc.)
    for expected in &[
        "bash",
        "read_file",
        "write_file",
        "str_replace",
        "list_dir",
        "grep",
        "glob",
        "git",
        "github",
        "memory",
        "session",
        "mo",
        "agent",
        "agent_job",
        "introspect",
        "lsp",
        "web_fetch",
        "web_search",
        "symbols",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing tool: {expected}"
        );
    }
}

#[test]
fn no_duplicate_tool_names() {
    let names: Vec<String> = all_tool_schemas()
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();
    let mut seen = std::collections::HashSet::new();
    for name in &names {
        assert!(seen.insert(name), "duplicate tool name: {name}");
    }
}

// ── TOOL_CATALOG ↔ schema consistency ────────────────────────────────────

#[test]
fn every_catalog_tool_has_schema() {
    // Tools with dynamically constructed schemas (not in static all_tool_schemas).
    // This list is self-validated below — if a tool listed here gains a static
    // schema or is removed from the catalog, the test will catch it.
    // `skill` embeds the live skill catalog and is generated per session.
    const DYNAMIC_SCHEMA_TOOLS: &[&str] = &["skill"];

    let schemas = all_tool_schemas();
    let schema_names: std::collections::HashSet<&str> = schemas
        .iter()
        .filter_map(|s| s["function"]["name"].as_str())
        .collect();

    // Validate the allowlist itself: every entry must exist in TOOL_CATALOG
    // and must NOT have a static schema (otherwise remove it from the list).
    let catalog_names: std::collections::HashSet<&str> = astra_runtime::tool_registry::TOOL_CATALOG
        .iter()
        .map(|t| t.name)
        .collect();
    for &dyn_tool in DYNAMIC_SCHEMA_TOOLS {
        assert!(
            catalog_names.contains(dyn_tool),
            "DYNAMIC_SCHEMA_TOOLS lists '{}' but it's not in TOOL_CATALOG — remove it",
            dyn_tool
        );
        assert!(
            !schema_names.contains(dyn_tool),
            "DYNAMIC_SCHEMA_TOOLS lists '{}' but it now has a static schema — remove it from the allowlist",
            dyn_tool
        );
    }

    for tool in astra_runtime::tool_registry::TOOL_CATALOG {
        if DYNAMIC_SCHEMA_TOOLS.contains(&tool.name) {
            continue;
        }
        assert!(
            schema_names.contains(tool.name),
            "TOOL_CATALOG has '{}' but no schema defined — add it to astra-tools/schemas.rs",
            tool.name
        );
    }
}

// NOTE: no reverse test (every schema → catalog entry) because TOOL_CATALOG
// is only for selection-eligible tools.  Many tools (ask_user, spawn_agent,
// plan-mode tools, etc.) have schemas but are dispatched directly without
// catalog-based selection.  The forward check above is the meaningful one.

// ── new tool schema coverage ──────────────────────────────────────────────

#[test]
fn schemas_include_consolidated_tools() {
    let schemas = all_tool_schemas();
    let names: Vec<&str> = schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    // Consolidated tools cover the old individual tools
    assert!(names.contains(&"git"), "missing git schema");
    assert!(names.contains(&"github"), "missing github schema");
    assert!(names.contains(&"lsp"), "missing lsp schema");
    assert!(names.contains(&"agent"), "missing agent schema");
}

// Transaction fields (transaction_id, rollback_on_failure) have been removed
// from tool schemas as part of the tool consolidation. Transaction support is
// now handled at the execution layer, not advertised per-schema.

#[test]
fn git_schema_has_stash_action() {
    let schemas = all_tool_schemas();
    let git_schema = schemas
        .iter()
        .find(|schema| schema["function"]["name"].as_str() == Some("git"))
        .expect("missing consolidated git schema");
    let actions = git_schema["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("missing git action enum");
    assert!(
        actions.iter().any(|v| v.as_str() == Some("stash")),
        "git schema should have stash action"
    );
    assert!(
        actions.iter().any(|v| v.as_str() == Some("revert_commit")),
        "git schema should have revert_commit action"
    );
}

// ── Conditional required (allOf/if-then) regression guards ──────────────
//
// Session 19ad8393 broke on `agent spawn` because the schema only
// declared `required: ["action"]` — the model omitted `description`
// (which the backend required) and all 4 calls failed. These tests
// verify the `allOf` blocks are present for every consolidated
// multi-action tool so we never regress to flat required again.

fn tool_schema<'a>(schemas: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    schemas
        .iter()
        .find(|s| s["function"]["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("missing schema: {name}"))
}

/// Returns the list of field names that are required when `action ==
/// target_action`, by inspecting the
/// `parameters["x-astra-per-action-required"]` extension map. This
/// replaces the previous `allOf` block — Anthropic/Bedrock reject
/// `allOf` at the top level of `input_schema`, so per-action
/// required fields live in a vendor-prefixed extension that
/// providers ignore but our prune code honours.
fn conditional_required_for(schema: &serde_json::Value, target_action: &str) -> Vec<String> {
    schema["function"]["parameters"]["x-astra-per-action-required"][target_action]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Anthropic/Bedrock Messages API rejects `tools[].input_schema`
/// that contains `allOf`, `oneOf`, or `anyOf` at the top level
/// (HTTP 400: "input_schema does not support oneOf, allOf, or anyOf
/// at the top level"). Regression guard: every tool schema's
/// `parameters` object must be free of these three keys.
#[test]
fn no_schema_uses_top_level_composition_keywords() {
    for schema in all_tool_schemas() {
        let name = schema["function"]["name"].as_str().unwrap_or("<unnamed>");
        let params = &schema["function"]["parameters"];
        for banned in &["allOf", "oneOf", "anyOf"] {
            assert!(
                params.get(*banned).is_none(),
                "tool `{name}` parameters contain top-level `{banned}`, which \
                 Anthropic/Bedrock reject with HTTP 400. Encode per-action required \
                 fields via the `x-astra-per-action-required` extension + description \
                 prose instead."
            );
        }
    }
}

#[test]
fn agent_spawn_schema_requires_description_and_prompt() {
    let schemas = all_tool_schemas();
    let req = conditional_required_for(tool_schema(&schemas, "agent"), "spawn");
    assert!(
        req.contains(&"description".to_string()),
        "agent.spawn must require `description`: {req:?}"
    );
    assert!(
        req.contains(&"prompt".to_string()),
        "agent.spawn must require `prompt`: {req:?}"
    );
}

#[test]
fn agent_other_actions_have_conditional_required() {
    let schemas = all_tool_schemas();
    let agent = tool_schema(&schemas, "agent");
    // `delegate` removed — it had no execution backend in CLI mode and
    // silently no-op'd. Schema enum + per-action required entries
    // both gone. See `agent_action_delegate_is_rejected_with_redirect_to_spawn`.
    assert_eq!(
        conditional_required_for(agent, "delegate"),
        Vec::<String>::new(),
        "delegate must NOT have a per-action-required entry — the action was removed"
    );
    assert_eq!(
        conditional_required_for(agent, "run_chain"),
        vec!["steps".to_string()]
    );
    assert_eq!(
        conditional_required_for(agent, "get_result"),
        vec!["agent_id".to_string()]
    );
    assert_eq!(
        conditional_required_for(agent, "send_message"),
        vec!["to".to_string(), "message".to_string()]
    );
}

#[test]
fn git_commit_and_revert_actions_declare_required_fields() {
    let schemas = all_tool_schemas();
    let git = tool_schema(&schemas, "git");
    assert_eq!(
        conditional_required_for(git, "commit"),
        vec!["message".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "revert_commit"),
        vec!["commit_sha".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "file_history"),
        vec!["file".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "log_search"),
        vec!["query".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "checkout_file"),
        vec!["path".to_string(), "ref".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "stash"),
        vec!["sub_action".to_string()]
    );
    assert_eq!(
        conditional_required_for(git, "worktree"),
        vec!["sub_action".to_string()]
    );
}

#[test]
fn github_schema_requires_pr_issue_numbers_and_title() {
    let schemas = all_tool_schemas();
    let gh = tool_schema(&schemas, "github");
    assert_eq!(
        conditional_required_for(gh, "get_pr"),
        vec!["pr_number".to_string()]
    );
    assert_eq!(
        conditional_required_for(gh, "ci_status"),
        vec!["pr_number".to_string()]
    );
    assert_eq!(
        conditional_required_for(gh, "get_issue"),
        vec!["issue_number".to_string()]
    );
    assert_eq!(
        conditional_required_for(gh, "create_issue"),
        vec!["title".to_string()]
    );
}

#[test]
fn memory_schema_requires_content_query_new_content_signal() {
    let schemas = all_tool_schemas();
    let mem = tool_schema(&schemas, "memory");
    assert_eq!(
        conditional_required_for(mem, "remember"),
        vec!["content".to_string()]
    );
    assert_eq!(
        conditional_required_for(mem, "recall"),
        vec!["query".to_string()]
    );
    assert_eq!(
        conditional_required_for(mem, "expand"),
        vec!["memory_id".to_string()]
    );
    assert_eq!(
        conditional_required_for(mem, "forget"),
        vec!["reason".to_string()]
    );
    assert_eq!(
        conditional_required_for(mem, "update"),
        vec!["reason".to_string()]
    );
    assert_eq!(
        conditional_required_for(mem, "feedback"),
        vec!["memory_id".to_string(), "signal".to_string()]
    );
}

#[test]
fn session_schema_requires_path_value_tool_query_and_not_ask_user() {
    let schemas = all_tool_schemas();
    let sess = tool_schema(&schemas, "session");
    assert_eq!(
        conditional_required_for(sess, "config"),
        vec!["path".to_string(), "value".to_string()]
    );
    assert_eq!(
        conditional_required_for(sess, "prioritize"),
        vec!["tool".to_string()]
    );
    assert_eq!(
        conditional_required_for(sess, "deprioritize"),
        vec!["tool".to_string()]
    );
    assert_eq!(
        conditional_required_for(sess, "ask_user"),
        Vec::<String>::new()
    );
    let actions = sess["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("session action enum");
    assert!(
        !actions.iter().any(|v| v.as_str() == Some("ask_user")),
        "ask_user must be a first-class tool, not a stale session action"
    );
}

#[test]
fn ask_user_schema_advertises_questionnaire_tabs_and_multi_select() {
    let schemas = all_tool_schemas();
    let ask = tool_schema(&schemas, "ask_user");
    let description = ask["function"]["description"]
        .as_str()
        .expect("ask_user description should be a string");
    assert!(description.contains("retry ask_user immediately"));
    let params = &ask["function"]["parameters"];
    assert!(
        params["required"]
            .as_array()
            .unwrap()
            .contains(&"questions".into())
    );
    let question = &params["properties"]["questions"]["items"];
    assert_eq!(question["properties"]["multi_select"]["type"], "boolean");
    assert_eq!(question["properties"]["allow_freeform"]["type"], "boolean");
    let choices = &question["properties"]["options"]["items"];
    assert!(
        choices.get("anyOf").is_some(),
        "options should accept either strings or described option objects"
    );
    let choice_object = &choices["anyOf"][1]["properties"];
    assert_eq!(choice_object["preview"]["type"], "string");
    assert_eq!(question["required"], serde_json::json!(["question"]));
}

#[test]
fn mo_schema_requires_sql_and_sub_action() {
    let schemas = all_tool_schemas();
    let mo = tool_schema(&schemas, "mo");
    assert_eq!(
        conditional_required_for(mo, "query"),
        vec!["sql".to_string()]
    );
    assert_eq!(
        conditional_required_for(mo, "snapshot"),
        vec!["sub_action".to_string()]
    );
    assert_eq!(
        conditional_required_for(mo, "branch"),
        vec!["sub_action".to_string()]
    );
}

#[test]
fn task_schema_requires_title_and_task_id() {
    let schemas = all_tool_schemas();
    let task = tool_schema(&schemas, "task");
    assert_eq!(
        conditional_required_for(task, "create"),
        vec!["title".to_string()]
    );
    assert_eq!(
        conditional_required_for(task, "update"),
        vec!["task_id".to_string()]
    );
    assert_eq!(
        conditional_required_for(task, "get"),
        vec!["task_id".to_string()]
    );
    assert_eq!(
        conditional_required_for(task, "stop"),
        vec!["task_id".to_string()]
    );
}

/// Phase 1 split: `task` is the durable checklist surface (claudecode v2
/// alignment). All background-execution actions live on a separate
/// `agent_job` tool (codex agent_jobs alignment). The `task` schema must
/// no longer advertise background_shell/background_agent/output/kill —
/// otherwise the model gets two equally-valid paths and picks the wrong
/// one for ordinary checklist work.
#[test]
fn task_schema_does_not_advertise_background_actions() {
    let schemas = all_tool_schemas();
    let task = tool_schema(&schemas, "task");
    let actions: Vec<&str> = task["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("task.action must be an enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for banned in &["background_shell", "background_agent", "output", "kill"] {
        assert!(
            !actions.contains(banned),
            "task.action enum still advertises `{banned}` — it must move to \
             the `agent_job` tool. Got: {actions:?}"
        );
    }
    // Sanity: the checklist verbs are still there.
    for kept in &["create", "update", "list", "get", "stop"] {
        assert!(
            actions.contains(kept),
            "task.action must still include `{kept}` — got: {actions:?}"
        );
    }
}

/// `agent_job` is the new home for background execution: shell processes
/// and durable agent runs. The actions mirror what was on `task` before
/// the split (background_shell/background_agent/output/kill) but with the
/// `background_` prefix dropped — there's no other kind of agent_job.
#[test]
fn agent_job_schema_exists_with_expected_actions() {
    let schemas = all_tool_schemas();
    let agent_job = schemas
        .iter()
        .find(|s| s["function"]["name"].as_str() == Some("agent_job"))
        .expect(
            "agent_job schema must exist — it's the new tool that owns \
             background_shell/background_agent/output/kill after the Phase 1 split",
        );
    let actions: Vec<&str> = agent_job["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("agent_job.action must be an enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for expected in &["shell", "agent", "output", "kill"] {
        assert!(
            actions.contains(expected),
            "agent_job.action must include `{expected}`. Got: {actions:?}"
        );
    }
}

#[test]
fn agent_job_schema_per_action_required_fields() {
    let schemas = all_tool_schemas();
    let agent_job = tool_schema(&schemas, "agent_job");
    assert_eq!(
        conditional_required_for(agent_job, "shell"),
        vec!["command".to_string()],
        "agent_job.shell must require `command` — without it the executor \
         has no command to run and the model would silently no-op"
    );
    assert_eq!(
        conditional_required_for(agent_job, "agent"),
        vec!["prompt".to_string()],
        "agent_job.agent must require `prompt`"
    );
    assert_eq!(
        conditional_required_for(agent_job, "output"),
        vec!["task_id".to_string()],
        "agent_job.output must require `task_id` — there's no implicit \
         most-recent-job; the model must name the job it's reading from"
    );
    assert_eq!(
        conditional_required_for(agent_job, "kill"),
        vec!["task_id".to_string()],
        "agent_job.kill must require `task_id` — never bulk-kill all jobs"
    );
}

// ── Plan mode surfaces ────────────────────────────────────────────────────
//
// Local CLI keeps `/plan` as the human entrypoint, but the model-facing local
// tool catalog now includes client-backed enter/exit wrappers so the active
// cloud plan lifecycle stays consistent across turns.

#[test]
fn local_cli_catalog_includes_plan_mode_wrappers() {
    let names: Vec<String> = crate::edge_tools::local_tool_schemas()
        .iter()
        .filter_map(|s| s["function"]["name"].as_str().map(ToString::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "enter_plan_mode"),
        "local CLI catalog should expose enter_plan_mode via the client-backed wrapper"
    );
    assert!(
        names.iter().any(|n| n == "exit_plan_mode"),
        "local CLI catalog should expose exit_plan_mode via the client-backed wrapper"
    );
}

#[test]
fn enter_plan_mode_and_exit_plan_mode_surface_with_plan_lifecycle() {
    use astra_turn_core::capability::{Capability, CapabilitySet};
    use astra_turn_core::tool_surface::{Surface, resolve};

    let pool = astra_tools::schemas::all_tool_schemas();
    let caps = CapabilitySet::empty().with(Capability::PlanLifecycle);
    let names: Vec<String> = resolve(Surface::Web, &caps, &pool)
        .into_iter()
        .filter_map(|schema| {
            schema
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(|name| name.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        names.contains(&"enter_plan_mode".to_string()),
        "PlanLifecycle capability must surface enter_plan_mode. Got: {names:?}"
    );
    assert!(
        names.contains(&"exit_plan_mode".to_string()),
        "PlanLifecycle capability must surface exit_plan_mode."
    );
}

#[test]
fn session_schema_no_longer_advertises_enter_plan_or_exit_plan() {
    // The local CLI keeps plan mode on `/plan`; session-tool plan actions
    // must stay absent so there is only one user-facing entrypoint.
    let schemas = all_tool_schemas();
    let session = tool_schema(&schemas, "session");
    let actions: Vec<&str> = session["function"]["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .expect("session.action must be an enum")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    for banned in &["enter_plan", "exit_plan"] {
        assert!(
            !actions.contains(banned),
            "session.action enum still advertises `{banned}` — it must move \
             to the top-level `{banned}_mode` tool. Got: {actions:?}"
        );
    }
}
