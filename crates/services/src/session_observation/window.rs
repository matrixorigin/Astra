//! Lossless text delivery within Memoria's LLM extraction input limits.
//! Progress addresses immutable filtered canonical text, never a copied outbox.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const MAX_MESSAGES: usize = 20;
const MAX_CHARS_PER_MESSAGE: usize = 500;
const MAX_INPUT_BYTES: usize = 6000;
const NEW_MESSAGES: usize = 12;
const NEW_BYTES: usize = 4000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationTextCursor {
    pub part: usize,
    pub byte_offset: usize,
}

struct TextPart<'a> {
    role: &'static str,
    text: &'a str,
}

pub struct ObservationInput<'a> {
    context: Vec<TextPart<'a>>,
    new: Vec<TextPart<'a>>,
}

pub struct ObservationWindow {
    pub messages: Vec<Map<String, Value>>,
    pub next: ObservationTextCursor,
    pub complete: bool,
}

fn text_parts(messages: &[Value]) -> Vec<TextPart<'_>> {
    fn collect<'a>(role: &'static str, content: &'a Value, out: &mut Vec<TextPart<'a>>) {
        match content {
            Value::String(text) if !text.is_empty() => out.push(TextPart { role, text }),
            Value::Array(blocks) => {
                for block in blocks {
                    if block.is_string() {
                        collect(role, block, out);
                    } else {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text" | "input_text" | "output_text") => {
                                if let Some(text) = block.get("text").and_then(Value::as_str)
                                    && !text.is_empty()
                                {
                                    out.push(TextPart { role, text });
                                }
                            }
                            Some("tool_result") => {
                                if let Some(content) = block.get("content") {
                                    collect(role, content, out);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    for message in messages {
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            Some("system") => "system",
            Some("developer") => "developer",
            Some("tool") => "tool",
            _ => "unknown",
        };
        if let Some(content) = message.get("content") {
            collect(role, content, &mut parts);
        }
    }
    parts
}

fn message(role: &str, text: &str) -> Map<String, Value> {
    Map::from_iter([
        ("role".into(), Value::String(role.into())),
        ("content".into(), Value::String(text.into())),
    ])
}

// Exact byte weight of the extractor's "[role]: content\n" rendering.
fn rendered_bytes(role: &str, text: &str) -> usize {
    role.len() + text.len() + 5
}

impl<'a> ObservationInput<'a> {
    pub fn new(context: &'a [Value], new: &'a [Value]) -> Self {
        Self {
            context: text_parts(context),
            new: text_parts(new),
        }
    }

    pub fn window(&self, cursor: ObservationTextCursor) -> Result<ObservationWindow, String> {
        if cursor.part > self.new.len()
            || (cursor.part == self.new.len() && cursor.byte_offset != 0)
            || self.new.get(cursor.part).is_some_and(|part| {
                cursor.byte_offset > part.text.len()
                    || !part.text.is_char_boundary(cursor.byte_offset)
            })
        {
            return Err("invalid canonical observation text cursor".into());
        }
        let mut next = cursor;
        let mut messages = Vec::new();
        let mut bytes = 0;
        while let Some(part) = self.new.get(next.part) {
            if next.byte_offset == part.text.len() {
                next.part += 1;
                next.byte_offset = 0;
                continue;
            }
            let remaining = &part.text[next.byte_offset..];
            let end = remaining
                .char_indices()
                .nth(MAX_CHARS_PER_MESSAGE)
                .map_or(remaining.len(), |(index, _)| index);
            let text = &remaining[..end];
            let weight = rendered_bytes(part.role, text);
            if messages.len() == NEW_MESSAGES || bytes + weight > NEW_BYTES {
                break;
            }
            bytes += weight;
            messages.push(message(part.role, text));
            next.byte_offset += end;
            if next.byte_offset == part.text.len() {
                next.part += 1;
                next.byte_offset = 0;
            }
        }
        // Context may be shortened; newly completed text never is. Include
        // preceding delivered text too, so references can cross batch boundaries.
        let mut context = Vec::new();
        let mut context_bytes = 0;
        let partial = self.new.get(cursor.part).and_then(|part| {
            (cursor.byte_offset > 0).then(|| (part.role, &part.text[..cursor.byte_offset]))
        });
        let preceding = partial
            .into_iter()
            .chain(
                self.new[..cursor.part]
                    .iter()
                    .rev()
                    .map(|p| (p.role, p.text)),
            )
            .chain(self.context.iter().rev().map(|p| (p.role, p.text)));
        if !messages.is_empty() {
            for (role, text) in preceding {
                if context.len() >= MAX_MESSAGES - NEW_MESSAGES
                    || context.len() + messages.len() >= MAX_MESSAGES
                {
                    break;
                }
                let max_bytes = (MAX_INPUT_BYTES - NEW_BYTES)
                    .saturating_sub(context_bytes + rendered_bytes(role, ""));
                if max_bytes == 0 {
                    break;
                }
                let char_start = text
                    .char_indices()
                    .rev()
                    .nth(MAX_CHARS_PER_MESSAGE - 1)
                    .map_or(0, |(index, _)| index);
                let mut start = char_start.max(text.len().saturating_sub(max_bytes));
                while !text.is_char_boundary(start) {
                    start += 1;
                }
                let tail = &text[start..];
                if tail.is_empty() {
                    break;
                }
                context_bytes += rendered_bytes(role, tail);
                context.push(message(role, tail));
            }
        }
        context.reverse();
        context.append(&mut messages);
        Ok(ObservationWindow {
            messages: context,
            next,
            complete: next.part == self.new.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backlog_and_multibyte_text_fit_the_real_extractor_without_losing_new_text() {
        let new = (0..35)
            .map(|index| json!({"role":"user", "content":format!("fact-{index}:{}", "记🙂".repeat(400))}))
            .collect::<Vec<_>>();
        let input = ObservationInput::new(&[], &new);
        let mut cursor = ObservationTextCursor::default();
        let mut delivered = vec![String::new(); new.len()];
        loop {
            let window = input.window(cursor).unwrap();
            assert!(window.next != cursor);
            assert!(window.messages.len() <= MAX_MESSAGES);
            let mut rendered = 0;
            for m in &window.messages {
                let text = m["content"].as_str().unwrap();
                assert!(text.chars().count() <= MAX_CHARS_PER_MESSAGE);
                rendered += rendered_bytes(m["role"].as_str().unwrap(), text);
            }
            assert!(rendered <= MAX_INPUT_BYTES);
            // Reconstruct the exact newly covered canonical range, independently
            // of the intentionally repeated reference context in the request.
            for (index, text) in delivered
                .iter_mut()
                .enumerate()
                .take(window.next.part.saturating_add(1).min(new.len()))
                .skip(cursor.part)
            {
                let source = new[index]["content"].as_str().unwrap();
                let start = if index == cursor.part {
                    cursor.byte_offset
                } else {
                    0
                };
                let end = if index == window.next.part {
                    window.next.byte_offset
                } else {
                    source.len()
                };
                let slice = &source[start..end];
                assert!(
                    window
                        .messages
                        .iter()
                        .filter_map(|m| m["content"].as_str())
                        .collect::<String>()
                        .contains(slice)
                );
                text.push_str(slice);
            }
            cursor = window.next;
            if window.complete {
                break;
            }
        }
        for (index, text) in delivered.iter().enumerate() {
            assert_eq!(text, new[index]["content"].as_str().unwrap());
        }
    }

    #[test]
    fn short_context_respects_the_reserved_reference_message_budget() {
        let context = (0..32)
            .map(|index| json!({"role":"user", "content":format!("context-{index}")}))
            .collect::<Vec<_>>();
        let new = vec![json!({"role":"user", "content":"new fact"})];
        let window = ObservationInput::new(&context, &new)
            .window(ObservationTextCursor::default())
            .unwrap();
        assert!(window.complete);
        assert_eq!(window.messages.len(), 9);
        assert_eq!(window.messages.first().unwrap()["content"], "context-24");
        assert_eq!(window.messages.last().unwrap()["content"], "new fact");
    }

    #[test]
    fn structured_text_and_partial_cursor_are_preserved() {
        let new = vec![
            json!({"role":"user","content":[{"type":"text","text":"甲🙂".repeat(400)},{"type":"image","text":"PRIVATE IMAGE STATE"},{"type":"thinking","text":"PRIVATE THOUGHT"},{"type":"text","text":"last fact"}]}),
        ];
        let input = ObservationInput::new(&[], &new);
        let first = input.window(ObservationTextCursor::default()).unwrap();
        assert!(first.complete);
        assert_eq!(first.messages.last().unwrap()["content"], "last fact");
        assert!(
            !first
                .messages
                .iter()
                .any(|m| m["content"].as_str().unwrap().contains("PRIVATE"))
        );
        assert!(
            input
                .window(ObservationTextCursor {
                    part: 0,
                    byte_offset: 1
                })
                .is_err()
        );
        assert!(
            input
                .window(ObservationTextCursor {
                    part: 4,
                    byte_offset: 0
                })
                .is_err()
        );
    }
}
