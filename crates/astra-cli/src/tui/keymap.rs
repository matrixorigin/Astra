use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppAction {
    ForceRedraw,
    ToggleTranscript,
}

pub(crate) struct AppKeymap;

impl AppKeymap {
    pub fn resolve(key: KeyEvent) -> Option<AppAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('l') if ctrl => Some(AppAction::ForceRedraw),
            KeyCode::Char('o' | 'O') if ctrl => Some(AppAction::ToggleTranscript),
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
        for code in ['o', 'O'] {
            let key = KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL);
            assert_eq!(AppKeymap::resolve(key), Some(AppAction::ToggleTranscript));
        }
    }

    #[test]
    fn ctrl_l_resolves_to_force_redraw() {
        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(AppKeymap::resolve(key), Some(AppAction::ForceRedraw));
    }

    #[test]
    fn navigation_keys_belong_to_the_focused_view() {
        for key in [
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
        ] {
            assert_eq!(AppKeymap::resolve(key), None);
        }
    }
}
