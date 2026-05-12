//! P0 contract: `tool_search` is a first-class tool, not an action inside
//! the `session` meta-tool. Having both confuses the LLM and produces
//! split-brain telemetry.

use astra_tools::schemas::all_tool_schemas;
use serde_json::Value;

#[test]
fn session_tool_enum_does_not_list_tool_search() {
    let schemas = all_tool_schemas();
    let session = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("session")
        })
        .expect("session schema present");

    let enum_vals = &session["function"]["parameters"]["properties"]["action"]["enum"];
    let actions: Vec<&str> = enum_vals
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();

    assert!(
        !actions.contains(&"tool_search"),
        "session.action must NOT list tool_search — there is a top-level tool_search tool now; got {actions:?}"
    );
}

#[test]
fn session_description_does_not_mention_tool_search() {
    let schemas = all_tool_schemas();
    let session = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("session")
        })
        .expect("session schema present");
    let desc = session["function"]["description"].as_str().unwrap();
    assert!(
        !desc.contains("tool_search"),
        "session description must not advertise tool_search; got: {desc}"
    );
}

#[test]
fn session_params_dont_mention_tool_search_params() {
    // query/max_results were session params only to support
    // action=tool_search. Removed along with the action.
    let schemas = all_tool_schemas();
    let session = schemas
        .iter()
        .find(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                == Some("session")
        })
        .expect("session schema present");
    let props = &session["function"]["parameters"]["properties"];
    let props_obj = props.as_object().unwrap();
    assert!(
        !props_obj.contains_key("query"),
        "`query` param on session was for tool_search; remove it"
    );
    assert!(
        !props_obj.contains_key("max_results"),
        "`max_results` on session was for tool_search; remove it"
    );
}
