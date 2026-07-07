use crossterm::style::Stylize;

/// Trait for abstracting terminal output from business logic.
/// Line-mode uses eprintln with color; TUI sends events through a channel.
pub(crate) trait ReplUiAdapter: Send {
    fn show_error(&mut self, msg: &str);
    fn show_warning(&mut self, msg: &str);
    fn show_info(&mut self, msg: &str);
    fn show_status(&mut self, msg: &str);
    fn blank_line(&mut self);
}

/// Line-mode adapter: writes to stderr with semantic color (existing behavior).
pub(crate) struct LineUiAdapter;

impl ReplUiAdapter for LineUiAdapter {
    fn show_error(&mut self, msg: &str) {
        eprintln!("{}", msg.red());
    }
    fn show_warning(&mut self, msg: &str) {
        eprintln!("{}", msg.yellow());
    }
    fn show_info(&mut self, msg: &str) {
        eprintln!("{msg}");
    }
    fn show_status(&mut self, msg: &str) {
        eprintln!("{}", msg.dim());
    }
    fn blank_line(&mut self) {
        eprintln!();
    }
}
