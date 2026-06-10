pub(crate) fn non_empty_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        if value.trim().is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

pub(crate) fn normalize_optional_string(value: &mut Option<String>) {
    if value
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        *value = None;
    }
}
