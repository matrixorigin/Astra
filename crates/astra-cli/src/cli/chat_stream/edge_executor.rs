use std::sync::OnceLock;

/// Stable executor identity for the local checkout (§5.5 `edge_executor_id`).
///
/// A one-shot CLI invocation is a short-lived process, but a Session may span
/// several such invocations. Deriving the default from the persisted physical
/// materialization keeps the executor identity across those process boundaries
/// while preserving `ASTRA_EDGE_EXECUTOR_ID` as the explicit label override.
static EDGE_EXECUTOR_INSTANCE_ID: OnceLock<Result<String, String>> = OnceLock::new();

pub(crate) fn edge_executor_instance_id() -> &'static str {
    match EDGE_EXECUTOR_INSTANCE_ID.get_or_init(resolve_edge_executor_instance_id) {
        Ok(id) => id.as_str(),
        Err(_) => "",
    }
}

pub(crate) fn try_edge_executor_instance_id() -> Result<&'static str, String> {
    match EDGE_EXECUTOR_INSTANCE_ID.get_or_init(resolve_edge_executor_instance_id) {
        Ok(id) => Ok(id.as_str()),
        Err(error) => Err(error.clone()),
    }
}

fn resolve_edge_executor_instance_id() -> Result<String, String> {
    if let Ok(id) = std::env::var("ASTRA_EDGE_EXECUTOR_ID") {
        let id = id.trim();
        if id.is_empty() {
            return Err("ASTRA_EDGE_EXECUTOR_ID must not be empty".into());
        }
        if !astra_runtime_env::is_valid_provider_id(id) {
            return Err(format!(
                "ASTRA_EDGE_EXECUTOR_ID is not a valid executor identity: {id}"
            ));
        }
        return Ok(id.to_string());
    }
    default_materialization_executor_id()
}

fn default_materialization_executor_id() -> Result<String, String> {
    let workspace = std::env::current_dir()
        .map_err(|error| format!("failed to resolve the CLI workspace directory: {error}"))?;
    let materialization_id = astra_runtime_env::load_or_create_materialization_id(&workspace)?;
    Ok(materialization_executor_id(&materialization_id))
}

fn materialization_executor_id(materialization_id: &str) -> String {
    format!("edge-materialization-{materialization_id}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn materialization_executor_id_is_stable_and_namespaced() {
        let first = super::materialization_executor_id("materialization-test");
        let second = super::materialization_executor_id("materialization-test");
        assert_eq!(first, second);
        assert!(first.starts_with("edge-materialization-"));
    }
}
