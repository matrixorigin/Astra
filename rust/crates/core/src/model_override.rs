/// Normalize a runtime model override at process/state boundaries.
///
/// `default` is a symbolic request to let the serving API choose its configured
/// default model. It is not a concrete model name and must not be persisted or
/// sent as a model override.
pub fn normalize_model_override(model: Option<&str>) -> Option<&str> {
    let model = model?.trim();
    if model.is_empty() || model.eq_ignore_ascii_case("default") {
        None
    } else {
        Some(model)
    }
}

pub fn normalize_model_override_owned(model: Option<String>) -> Option<String> {
    normalize_model_override(model.as_deref()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbolic_default_is_not_a_runtime_override() {
        assert_eq!(normalize_model_override(None), None);
        assert_eq!(normalize_model_override(Some("")), None);
        assert_eq!(normalize_model_override(Some(" default ")), None);
        assert_eq!(normalize_model_override(Some("DEFAULT")), None);
        assert_eq!(
            normalize_model_override(Some("MiniMax-M2.7")),
            Some("MiniMax-M2.7")
        );
        assert_eq!(
            normalize_model_override_owned(Some(" gpt-5.5 ".to_string())),
            Some("gpt-5.5".to_string())
        );
    }
}
