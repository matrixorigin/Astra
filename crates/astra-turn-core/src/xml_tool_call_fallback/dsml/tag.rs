//! Incremental grammar for reserved DSML tags. Both complete and fragmented
//! input take this path; each byte is inspected once while a tag is pending.

use super::Kind;

const MARKERS: [&str; 4] = ["｜DSML｜", "｜｜DSML｜｜", "|DSML|", "||DSML||"];
const NAMES: [(&str, Kind); 5] = [
    ("tool_calls", Kind::Calls),
    ("function_calls", Kind::Calls),
    ("calls", Kind::Calls),
    ("invoke", Kind::Invoke),
    ("parameter", Kind::Parameter),
];

#[derive(Debug, Default)]
enum Stage {
    #[default]
    Slash,
    Marker,
    MarkerWord {
        candidates: u8,
        position: usize,
    },
    Name,
    NameWord {
        candidates: u8,
        position: usize,
    },
    Attributes,
}

#[derive(Debug, Default)]
pub(super) struct Scanner {
    checked: usize,
    stage: Stage,
    closing: bool,
    reserved: bool,
    kind: Option<Kind>,
}

pub(super) enum Scan {
    Pending,
    Ordinary,
    Incomplete,
    Tag {
        end: usize,
        kind: Kind,
        closing: bool,
    },
}

impl Scanner {
    pub(super) fn scan(&mut self, text: &str, finish: bool) -> Scan {
        if self.checked == 0 {
            debug_assert!(text.starts_with('<'));
            self.checked = 1;
        }
        for (index, ch) in text[self.checked..].char_indices() {
            let end = self.checked + index + ch.len_utf8();
            match &mut self.stage {
                Stage::Slash if ch.is_whitespace() => {}
                Stage::Slash if ch == '/' => {
                    self.closing = true;
                    self.stage = Stage::Marker;
                }
                Stage::Slash | Stage::Marker => {
                    if ch.is_whitespace() {
                        continue;
                    }
                    let candidates = MARKERS.iter().enumerate().fold(0, |bits, (i, marker)| {
                        bits | if marker.starts_with(ch) { 1 << i } else { 0 }
                    });
                    if candidates == 0 {
                        return Scan::Ordinary;
                    }
                    self.reserved = true;
                    self.stage = Stage::MarkerWord {
                        candidates,
                        position: 1,
                    };
                }
                Stage::MarkerWord {
                    candidates,
                    position,
                } => {
                    let mut remaining = 0;
                    let mut complete = false;
                    for (i, marker) in MARKERS.iter().enumerate() {
                        if *candidates & (1 << i) != 0
                            && marker
                                .chars()
                                .nth(*position)
                                .is_some_and(|expected| expected.eq_ignore_ascii_case(&ch))
                        {
                            remaining |= 1 << i;
                            complete |= marker.chars().count() == *position + 1;
                        }
                    }
                    if remaining == 0 {
                        return Scan::Ordinary;
                    }
                    if complete {
                        self.stage = Stage::Name;
                    } else {
                        *candidates = remaining;
                        *position += 1;
                    }
                }
                Stage::Name if ch.is_whitespace() => {}
                Stage::Name => {
                    let candidates = NAMES.iter().enumerate().fold(0, |bits, (i, (name, _))| {
                        bits | if name.chars().next().unwrap().eq_ignore_ascii_case(&ch) {
                            1 << i
                        } else {
                            0
                        }
                    });
                    if candidates == 0 {
                        return Scan::Ordinary;
                    }
                    self.stage = Stage::NameWord {
                        candidates,
                        position: 1,
                    };
                }
                Stage::NameWord {
                    candidates,
                    position,
                } => {
                    if ch.is_whitespace() || matches!(ch, '/' | '>') {
                        let Some((_, kind)) =
                            NAMES.iter().enumerate().find_map(|(i, (name, kind))| {
                                (*candidates & (1 << i) != 0 && name.len() == *position)
                                    .then_some((name, *kind))
                            })
                        else {
                            return Scan::Ordinary;
                        };
                        self.kind = Some(kind);
                        if ch == '>' {
                            return Scan::Tag {
                                end,
                                kind,
                                closing: self.closing,
                            };
                        }
                        self.stage = Stage::Attributes;
                    } else {
                        let remaining = NAMES.iter().enumerate().fold(0, |bits, (i, (name, _))| {
                            bits | if *candidates & (1 << i) != 0
                                && name.as_bytes().get(*position).is_some_and(|expected| {
                                    ch.is_ascii() && expected.eq_ignore_ascii_case(&(ch as u8))
                                }) {
                                1 << i
                            } else {
                                0
                            }
                        });
                        if remaining == 0 {
                            return Scan::Ordinary;
                        }
                        *candidates = remaining;
                        *position += 1;
                    }
                }
                Stage::Attributes if ch == '<' => return Scan::Ordinary,
                Stage::Attributes if ch == '>' => {
                    return Scan::Tag {
                        end,
                        kind: self.kind.unwrap(),
                        closing: self.closing,
                    };
                }
                Stage::Attributes => {}
            }
        }
        self.checked = text.len();
        if finish {
            if self.reserved {
                Scan::Incomplete
            } else {
                Scan::Ordinary
            }
        } else {
            Scan::Pending
        }
    }
}
