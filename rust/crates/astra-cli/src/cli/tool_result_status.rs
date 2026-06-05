use crate::cli::theme;
pub(crate) use astra_tools::tool_result_status::{
    tool_result_status_is_failure, tool_result_status_is_success,
};

pub(crate) fn tool_result_status_icon(status: &str) -> String {
    if tool_result_status_is_failure(status) {
        theme::icon_err()
    } else {
        theme::icon_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_uses_failure_semantics_for_non_success_status() {
        assert_eq!(tool_result_status_icon("ok"), theme::icon_ok());
        assert_eq!(tool_result_status_icon("success"), theme::icon_ok());
        assert_eq!(
            tool_result_status_icon("permission_denied"),
            theme::icon_err()
        );
    }
}
