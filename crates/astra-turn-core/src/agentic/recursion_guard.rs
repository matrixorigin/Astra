pub const MAX_AGENT_RECURSION_DEPTH: u8 = 3;

pub fn recursion_depth_limit_error(current_depth: u32) -> String {
    format!(
        "recursion depth {current_depth} reached maximum {MAX_AGENT_RECURSION_DEPTH}; nested delegations, skill forks, and spawned agents are disabled"
    )
}

pub fn checked_child_recursion_depth(current_depth: u8) -> Result<u8, String> {
    checked_child_recursion_depth_u32(u32::from(current_depth))
}

pub fn checked_child_recursion_depth_u32(current_depth: u32) -> Result<u8, String> {
    if current_depth >= u32::from(MAX_AGENT_RECURSION_DEPTH) {
        return Err(recursion_depth_limit_error(current_depth));
    }
    Ok((current_depth + 1) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_child_depth_up_to_limit() {
        assert_eq!(checked_child_recursion_depth(0).unwrap(), 1);
        assert_eq!(checked_child_recursion_depth(2).unwrap(), 3);
    }

    #[test]
    fn rejects_children_past_limit() {
        let err = checked_child_recursion_depth(3).unwrap_err();
        assert!(err.contains("recursion depth 3 reached maximum 3"));
    }
}
