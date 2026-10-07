//! Edge capability facts attached to Server admission requests.

use std::collections::HashSet;

use serde_json::Value;

/// Shallow-merge top-level keys from `extensions` into `edge_profile` (cloud–edge audit / lineage).
///
/// `extensions` must be a JSON object. Non-object values are ignored.
pub fn merge_edge_profile_extensions(payload: &mut Value, extensions: &Value) {
    let Some(ext_obj) = extensions.as_object() else {
        return;
    };
    if ext_obj.is_empty() {
        return;
    }
    if let Some(root) = payload.as_object_mut()
        && let Some(ep) = root.get_mut("edge_profile")
        && let Some(ep_obj) = ep.as_object_mut()
    {
        for (k, v) in ext_obj {
            ep_obj.insert(k.clone(), v.clone());
        }
    }
}

/// Dynamic tool schemas for this turn (`edge_tools`).
pub fn set_payload_edge_tools(payload: &mut Value, schemas: Vec<Value>) {
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("edge_tools".to_string(), Value::Array(schemas));
    }
}

/// Drop schemas whose `function.name` is in `restricted_tools`, then set
/// `edge_tools` on the payload. The runtime-owned deferred invocation carrier
/// is a protocol primitive rather than a provider capability, so it remains
/// available whenever the caller assembled it; otherwise a visible
/// `tool_search` plus a deferred manifest would have no executable next step.
pub fn attach_filtered_edge_tools(
    payload: &mut Value,
    turn_schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
) {
    let final_schemas = turn_schemas
        .into_iter()
        .filter(|schema| {
            crate::tool::schema::tool_schema_name(schema).is_none_or(|name| {
                name == crate::tool::deferred_activation::DEFERRED_TOOL_INVOCATION_CARRIER
                    || !restricted_tools.contains(name)
            })
        })
        .collect();
    set_payload_edge_tools(payload, final_schemas);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn merge_edge_profile_extensions_merges_objects() {
        let mut p = json!({ "edge_profile": { "cwd": "/tmp", "k": 1 } });
        merge_edge_profile_extensions(
            &mut p,
            &json!({
                "session_lineage": { "parent_session_id": "abc" },
                "edge_policy": { "permission_mode": "prompt" }
            }),
        );
        assert_eq!(p["edge_profile"]["cwd"], "/tmp");
        assert_eq!(p["edge_profile"]["k"], 1);
        assert_eq!(
            p["edge_profile"]["session_lineage"]["parent_session_id"],
            "abc"
        );
        assert_eq!(
            p["edge_profile"]["edge_policy"]["permission_mode"],
            "prompt"
        );
    }

    #[test]
    fn set_payload_edge_tools_attaches_schema() {
        let mut p = json!({});
        set_payload_edge_tools(&mut p, vec![json!({"fn": "t1"})]);
        assert_eq!(p["edge_tools"], json!([{"fn": "t1"}]));
    }

    #[test]
    fn attach_filtered_edge_tools_excludes_by_name() {
        let mut p = json!({});
        let schemas = vec![
            json!({"function": {"name": "bash"}}),
            json!({"function": {"name": "danger"}}),
        ];
        let mut r = HashSet::new();
        r.insert("danger".into());
        attach_filtered_edge_tools(&mut p, schemas, &r);
        let arr = p["edge_tools"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["function"]["name"], "bash");
    }

    #[test]
    fn attach_filtered_edge_tools_keeps_runtime_carrier_outside_capability_restrictions() {
        let mut p = json!({});
        let carrier = crate::tool::deferred_activation::deferred_tool_invocation_carrier_schema();
        let schemas = vec![json!({"function": {"name": "tool_search"}}), carrier];
        let restricted = HashSet::from([
            "tool_search".to_string(),
            crate::tool::deferred_activation::DEFERRED_TOOL_INVOCATION_CARRIER.to_string(),
        ]);

        attach_filtered_edge_tools(&mut p, schemas, &restricted);

        let names = p["edge_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(crate::tool::schema::tool_schema_name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["invoke_tool"]);
    }
}
