/// Normalize a runtime model override at process/state boundaries.
///
/// `default` is a symbolic request to let the serving API choose its configured
/// default model. It is not a concrete model name and must not be persisted or
/// sent as a model override.
pub const MISSING_MODEL_SELECTION_MESSAGE: &str = "\
Model selection is required. Select a concrete model with `/model set <name>`, \
pass `--model <name>`, or run `astra config set default_model <name>`.";

pub fn normalize_model_override(model: Option<&str>) -> Option<&str> {
    let model = model?.trim();
    if model.is_empty()
        || model.eq_ignore_ascii_case("default")
        || model.chars().any(char::is_control)
    {
        None
    } else {
        Some(model)
    }
}

pub fn normalize_model_override_owned(model: Option<String>) -> Option<String> {
    normalize_model_override(model.as_deref()).map(str::to_string)
}

pub fn missing_model_selection_error() -> crate::ClassifiedError {
    crate::ClassifiedError::new(
        crate::ErrorKind::MissingModelSelection,
        MISSING_MODEL_SELECTION_MESSAGE,
    )
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
        assert_eq!(normalize_model_override(Some("bad\nmodel")), None);
        assert_eq!(normalize_model_override(Some("bad\tmodel")), None);
        assert_eq!(
            normalize_model_override(Some("MiniMax-M2.7")),
            Some("MiniMax-M2.7")
        );
        assert_eq!(
            normalize_model_override_owned(Some(" gpt-5.5 ".to_string())),
            Some("gpt-5.5".to_string())
        );
    }

    #[test]
    fn missing_model_selection_error_is_classified() {
        let err = missing_model_selection_error();

        assert_eq!(err.kind, crate::ErrorKind::MissingModelSelection);
        assert!(err.message.contains("default_model"));
    }
}
