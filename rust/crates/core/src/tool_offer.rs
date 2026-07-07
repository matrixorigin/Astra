/// Build the canonical runtime offer id for one concrete provider offer.
///
/// Offer ids are runtime/control-plane identifiers. They must not be embedded
/// into prompt-visible tool schemas.
pub fn tool_offer_id(tool_name: &str, provider_id: &str) -> String {
    assert!(
        is_valid_tool_offer_tool_name(tool_name),
        "invalid tool name for offer id: {tool_name}"
    );
    assert!(
        is_valid_provider_id(provider_id),
        "invalid provider id for offer id: {provider_id}"
    );
    format!("{tool_name}@{provider_id}")
}

pub const MCP_NAMESPACED_TOOL_PREFIX: &str = "mcp__";

/// Returns true when `value` is a syntactically valid MCP namespaced canonical
/// tool name such as `mcp__github__search`.
///
/// This is only a name-shape predicate. It does not mean an MCP provider is
/// configured, ready, selected, or authorized for the current scope.
pub fn is_mcp_namespaced_tool_name(value: &str) -> bool {
    value
        .strip_prefix(MCP_NAMESPACED_TOOL_PREFIX)
        .is_some_and(|suffix| {
            suffix.bytes().any(|byte| byte.is_ascii_alphanumeric())
                && is_valid_tool_offer_tool_name(value)
        })
}

pub fn is_valid_tool_offer_id(value: &str) -> bool {
    let Some((tool_name, provider_id)) = value.split_once('@') else {
        return false;
    };
    !provider_id.contains('@')
        && is_valid_tool_offer_tool_name(tool_name)
        && is_valid_provider_id(provider_id)
}

pub fn is_valid_provider_id(value: &str) -> bool {
    is_valid_identifier_part(value, IdentifierPart::Provider)
}

pub fn is_valid_tool_offer_tool_name(value: &str) -> bool {
    is_valid_identifier_part(value, IdentifierPart::Tool)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentifierPart {
    Tool,
    Provider,
}

fn is_valid_identifier_part(value: &str, part: IdentifierPart) -> bool {
    !value.is_empty()
        && value.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-')
                || (part == IdentifierPart::Provider && matches!(byte, b'.' | b':'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_identifiers_accept_canonical_tool_and_provider_ids() {
        assert!(is_valid_provider_id("server-builtin"));
        assert!(is_valid_provider_id("edge:macpro.local"));
        assert!(is_valid_tool_offer_id(
            "mcp__github__search@request-scoped-mcp"
        ));
        assert!(is_valid_tool_offer_id("bash@edge:macpro.local"));
        assert_eq!(
            tool_offer_id("web_fetch", "server-builtin"),
            "web_fetch@server-builtin"
        );
    }

    #[test]
    fn mcp_namespaced_tool_name_is_only_a_valid_name_shape() {
        assert!(is_mcp_namespaced_tool_name("mcp__github__search"));
        assert!(is_mcp_namespaced_tool_name("mcp__local_docs__query"));
        assert!(!is_mcp_namespaced_tool_name("web_fetch"));
        assert!(!is_mcp_namespaced_tool_name("mcp__bad/name"));
        assert!(!is_mcp_namespaced_tool_name("mcp__"));
    }

    #[test]
    fn offer_identifiers_reject_ambiguous_or_path_like_values() {
        assert!(!is_valid_provider_id("edge@macpro"));
        assert!(!is_valid_provider_id("edge macpro"));
        assert!(!is_valid_provider_id("../edge"));
        assert!(!is_valid_provider_id("..."));
        assert!(!is_valid_tool_offer_id("web_fetch"));
        assert!(!is_valid_tool_offer_id("web_fetch@edge@macpro"));
        assert!(!is_valid_tool_offer_id("web.fetch@server-builtin"));
        assert!(!is_valid_tool_offer_id("web_fetch@edge/macpro"));
    }

    #[test]
    #[should_panic(expected = "invalid provider id for offer id")]
    fn tool_offer_id_rejects_invalid_provider_ids_in_release() {
        let _ = tool_offer_id("web_fetch", "edge@macpro");
    }

    #[test]
    #[should_panic(expected = "invalid tool name for offer id")]
    fn tool_offer_id_rejects_invalid_tool_names_in_release() {
        let _ = tool_offer_id("web.fetch", "server-builtin");
    }
}
