//! Encoding helpers for values embedded in URL path/query components.
//!
//! This deliberately follows RFC 3986 component encoding rather than HTML
//! form encoding: spaces become `%20`, and every reserved byte is escaped.
//! It is therefore safe to interpolate the output into one query value
//! without allowing the value to create another query parameter.

/// Percent-encode one RFC 3986 URL component using uppercase hexadecimal.
///
/// Only the unreserved byte set (`A-Z`, `a-z`, `0-9`, `-`, `.`, `_`, `~`) is
/// left unchanged. UTF-8 input is encoded byte-for-byte, preserving the exact
/// original value when a URL parser decodes the component.
#[must_use]
pub fn encode_url_component(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::encode_url_component;

    #[test]
    fn encodes_reserved_bytes_without_creating_query_structure() {
        assert_eq!(
            encode_url_component("why?source_policy=cloud_only% +/雪"),
            "why%3Fsource_policy%3Dcloud_only%25%20%2B%2F%E9%9B%AA"
        );
    }

    #[test]
    fn preserves_rfc3986_unreserved_bytes() {
        assert_eq!(encode_url_component("AZaz09-._~"), "AZaz09-._~");
    }
}
