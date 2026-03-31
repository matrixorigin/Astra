//! Serialize the `explain` field for edge `/chat` JSON payloads (thin-client protocol).

use serde_json::{Value, json};

/// Build the `explain` field: `false` | `true` | `"verbose"`.
///
/// Callers map their enum to the two booleans (`verbose` wins over `on`).
#[must_use]
pub fn chat_turn_explain_field_json(verbose: bool, on: bool) -> Value {
    if verbose {
        json!("verbose")
    } else if on {
        json!(true)
    } else {
        json!(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_is_false() {
        assert_eq!(chat_turn_explain_field_json(false, false), json!(false));
    }

    #[test]
    fn on_is_true() {
        assert_eq!(chat_turn_explain_field_json(false, true), json!(true));
    }

    #[test]
    fn verbose_string() {
        assert_eq!(chat_turn_explain_field_json(true, false), json!("verbose"));
    }

    #[test]
    fn verbose_over_on() {
        assert_eq!(chat_turn_explain_field_json(true, true), json!("verbose"));
    }
}
