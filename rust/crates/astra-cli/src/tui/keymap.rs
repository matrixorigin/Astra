use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppAction {
    ScrollPageUp,
    ScrollPageDown,
    JumpToBottom,
    JumpToTop,
    ForceRedraw,
    ToggleTranscript,
}

pub(crate) struct AppKeymap;

impl AppKeymap {
    pub fn resolve(key: KeyEvent) -> Option<AppAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('l') if ctrl => Some(AppAction::ForceRedraw),
            KeyCode::Char('o') if ctrl => Some(AppAction::ToggleTranscript),
            KeyCode::PageUp => Some(AppAction::ScrollPageUp),
            KeyCode::PageDown => Some(AppAction::ScrollPageDown),
            KeyCode::Home if ctrl => Some(AppAction::JumpToTop),
            KeyCode::End if ctrl => Some(AppAction::JumpToBottom),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppAction, AppKeymap};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn ctrl_o_resolves_to_transcript_toggle() {
        let key = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(AppKeymap::resolve(key), Some(AppAction::ToggleTranscript));
    }
}
