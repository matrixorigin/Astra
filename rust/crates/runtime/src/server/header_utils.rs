use std::collections::HashMap;

use axum::http::{HeaderMap, HeaderName, HeaderValue};

pub(super) fn normalize_forward_header(
    name: &HeaderName,
    value: &HeaderValue,
) -> Option<(String, String)> {
    value
        .to_str()
        .ok()
        .map(|raw| (name.as_str().to_ascii_lowercase(), raw.to_string()))
}

pub(super) fn collect_forward_headers(headers: &HeaderMap) -> HashMap<String, String> {
    let mut forwarded_headers = HashMap::new();

    for (name, value) in headers.iter() {
        if let Some((name, value)) = normalize_forward_header(name, value) {
            forwarded_headers.entry(name).or_insert(value);
        }
    }

    forwarded_headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_forward_headers_keeps_first_duplicate_value() {
        let mut headers = HeaderMap::new();
        headers.append(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer first"),
        );
        headers.append(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer second"),
        );

        let collected = collect_forward_headers(&headers);
        assert_eq!(
            collected.get("authorization").map(String::as_str),
            Some("Bearer first")
        );
    }
}
