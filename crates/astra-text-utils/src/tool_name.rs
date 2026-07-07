pub fn normalize_ascii_tool_name(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_ascii_tool_name;

    #[test]
    fn normalize_ascii_tool_name_trims_and_rejects_blank() {
        assert_eq!(
            normalize_ascii_tool_name(" Send_Message "),
            Some("send_message".to_string())
        );
        assert_eq!(normalize_ascii_tool_name(" \t "), None);
    }
}
