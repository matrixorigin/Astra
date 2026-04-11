//! Helpers for re-rendering slash picker submissions in the prompt.

pub(crate) fn should_clear_picker_submission_echo(
    line: &str,
    pending_execute: Option<&str>,
) -> bool {
    pending_execute.is_some_and(|cmd| cmd != line)
}

pub(crate) fn build_picker_submission_echo(prompt: &str, actual_cmd: &str) -> String {
    format!("\x1b[A\x1b[2K\r{}{actual_cmd}\n", prompt)
}

/// Clear the readline echo line and re-print with the actual dispatched command.
pub(crate) fn replace_picker_submission_echo(prompt: &str, actual_cmd: &str) {
    use std::io::Write;
    print!("{}", build_picker_submission_echo(prompt, actual_cmd));
    let _ = std::io::stdout().flush();
}
