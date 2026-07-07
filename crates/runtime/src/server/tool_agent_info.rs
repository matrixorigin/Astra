use serde_json::{Value, json};

pub(crate) struct AgentInfoIdentity<'a> {
    pub(crate) name: &'a str,
    pub(crate) version: &'a str,
    pub(crate) runtime: &'a str,
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) workspace: String,
}

fn identity_json(identity: AgentInfoIdentity<'_>) -> Value {
    json!({
        "name": identity.name,
        "version": identity.version,
        "runtime": identity.runtime,
        "user_id": identity.user_id,
        "session_id": identity.session_id,
        "workspace": identity.workspace,
    })
}

fn capability_json(tool_names: &[&str]) -> Value {
    json!({
        "tool_count": tool_names.len(),
        "tools": tool_names,
    })
}

pub(crate) fn render_agent_info(
    args: &Value,
    identity: AgentInfoIdentity<'_>,
    tool_names: &[&str],
) -> String {
    let dimension = args
        .get("dimension")
        .and_then(Value::as_str)
        .unwrap_or("all");

    match dimension {
        "identity" => identity_json(identity).to_string(),
        "capability" => capability_json(tool_names).to_string(),
        _ => {
            let mut info = identity_json(identity);
            if let Value::Object(ref mut fields) = info
                && let Value::Object(capability_fields) = capability_json(tool_names)
            {
                fields.extend(capability_fields);
            }
            info.to_string()
        }
    }
}
