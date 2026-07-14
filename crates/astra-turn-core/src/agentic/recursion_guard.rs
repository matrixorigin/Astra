/// Absolute process safety ceiling for recursive agent execution.
///
/// Product policy remains profile-scoped (`max_delegation_depth`, whose
/// default is lower). This ceiling exists only to bound malformed or
/// internally spawned recursion that has no profile policy available.
pub const ABSOLUTE_MAX_AGENT_RECURSION_DEPTH: u8 = 8;

pub fn recursion_depth_limit_error(current_depth: u32) -> String {
    format!(
        "recursion depth {current_depth} reached absolute safety ceiling {ABSOLUTE_MAX_AGENT_RECURSION_DEPTH}; nested delegations, skill forks, and spawned agents are disabled"
    )
}

pub fn checked_child_recursion_depth(current_depth: u8) -> Result<u8, String> {
    checked_child_recursion_depth_u32(u32::from(current_depth))
}

pub fn checked_child_recursion_depth_u32(current_depth: u32) -> Result<u8, String> {
    if current_depth >= u32::from(ABSOLUTE_MAX_AGENT_RECURSION_DEPTH) {
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
        assert_eq!(checked_child_recursion_depth(7).unwrap(), 8);
    }

    #[test]
    fn rejects_children_past_limit() {
        let err = checked_child_recursion_depth(8).unwrap_err();
        assert!(err.contains("recursion depth 8 reached absolute safety ceiling 8"));
    }
}
