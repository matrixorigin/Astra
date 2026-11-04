//! Incremental Markdown line/container prefixes used by the DSML lexer.
//! Cursor checkpoints retain completed containers. Only bounded marker/padding
//! lookahead can be revisited; long indentation is published once it is literal.

#[derive(Clone, Debug)]
pub(super) enum Container {
    Quote,
    List(usize),
}

pub(super) struct Prefix {
    pub end: usize,
    pub indented: bool,
    pub containers: Vec<Container>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Position {
    byte: usize,
    column: usize,
    carry: usize,
}

impl Position {
    fn indent(&mut self, bytes: &[u8], limit: usize) -> usize {
        let mut width = std::mem::take(&mut self.carry);
        while width < limit {
            let step = match bytes.get(self.byte) {
                Some(b' ') => 1,
                Some(b'\t') => 4 - self.column % 4,
                _ => break,
            };
            self.byte += 1;
            self.column += step;
            width += step;
        }
        width
    }

    fn marker(&mut self, width: usize) {
        self.byte += width;
        self.column += width;
    }

    // A tab after `>` supplies one padding column; any remaining columns are
    // indentation within the quote, not a fresh four-column tab expansion.
    fn padding_column(&mut self, bytes: &[u8]) {
        match bytes.get(self.byte) {
            Some(b' ') => self.marker(1),
            Some(b'\t') => {
                let width = 4 - self.column % 4;
                self.byte += 1;
                self.column += width;
                self.carry = width - 1;
            }
            _ => {}
        }
    }

    fn quote(&mut self, bytes: &[u8], finish: bool) -> Option<bool> {
        if bytes.get(self.byte) != Some(&b'>') {
            return Some(false);
        }
        if self.byte + 1 == bytes.len() && !finish {
            return None;
        }
        self.marker(1);
        self.padding_column(bytes);
        Some(true)
    }
}

#[derive(Debug, Default)]
pub(super) struct Cursor {
    at: Position,
    containers: Vec<Container>,
    next: usize,
}

impl Cursor {
    fn prefix(at: Position, indented: bool, containers: &[Container]) -> Prefix {
        Prefix {
            end: at.byte,
            indented,
            containers: containers.to_vec(),
        }
    }

    pub(super) fn normal(&mut self, text: &str, finish: bool) -> Option<Prefix> {
        let bytes = text.as_bytes();
        loop {
            let mut at = self.at;
            let indent = at.indent(bytes, 4);
            // Four columns already establish literal indentation. Do not wait
            // for the end of an arbitrarily long whitespace-only stream.
            if indent >= 4 {
                return Some(Self::prefix(at, true, &self.containers));
            }
            if at.byte == bytes.len() && !finish {
                return None;
            }
            if at.quote(bytes, finish)? {
                self.containers.push(Container::Quote);
                self.at = at;
                continue;
            }
            let marker = match bytes.get(at.byte) {
                Some(b'-' | b'+' | b'*') => Some(1),
                Some(b'0'..=b'9') => {
                    let digits = bytes[at.byte..]
                        .iter()
                        .take(10)
                        .take_while(|ch| ch.is_ascii_digit())
                        .count();
                    if at.byte + digits == bytes.len() && digits <= 9 && !finish {
                        return None;
                    }
                    (digits <= 9 && matches!(bytes.get(at.byte + digits), Some(b'.' | b')')))
                        .then_some(digits + 1)
                }
                _ => None,
            };
            if let Some(marker) = marker {
                let mut content = at;
                content.marker(marker);
                if content.byte == bytes.len() && !finish {
                    return None;
                }
                if matches!(bytes.get(content.byte), Some(b' ' | b'\t')) {
                    let padding = content.indent(bytes, 5);
                    if padding < 5 && content.byte == bytes.len() && !finish {
                        return None;
                    }
                    let width = if padding >= 5 {
                        content = at;
                        content.marker(marker);
                        content.padding_column(bytes);
                        1
                    } else {
                        padding
                    };
                    self.containers
                        .push(Container::List(indent + marker + width));
                    self.at = content;
                    continue;
                }
            }
            return Some(Self::prefix(at, false, &self.containers));
        }
    }

    /// Only the opening containers are consumed inside a fence. Blank lines
    /// may omit list padding; they do not leave the list or close its fence.
    /// None needs more input, Some(None) has left the containing block.
    pub(super) fn fence(
        &mut self,
        text: &str,
        finish: bool,
        containers: &[Container],
    ) -> Option<Option<Prefix>> {
        let bytes = text.as_bytes();
        for (i, container) in containers.iter().enumerate().skip(self.next) {
            let mut at = self.at;
            match container {
                Container::Quote => {
                    if at.indent(bytes, 4) > 3 {
                        return Some(None);
                    }
                    if at.byte == bytes.len() && !finish {
                        return None;
                    }
                    if !at.quote(bytes, finish)? {
                        return Some(None);
                    }
                }
                Container::List(width) => {
                    let padding = at.indent(bytes, *width);
                    if padding < *width {
                        if at.byte == bytes.len() && !finish {
                            return None;
                        }
                        if at.byte == bytes.len() || bytes.get(at.byte) == Some(&b'\n') {
                            return Some(Some(Self::prefix(at, false, containers)));
                        }
                        return Some(None);
                    }
                    at.carry = padding - width;
                }
            }
            self.at = at;
            self.next = i + 1;
        }
        let mut at = self.at;
        let indent = at.indent(bytes, 4);
        if indent < 4 && at.byte == bytes.len() && !finish {
            return None;
        }
        Some(Some(Self::prefix(at, indent >= 4, containers)))
    }
}
