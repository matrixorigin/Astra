use super::*;

// ── UTF-16 conversion tests ───────────────────────────────────────────────

#[test]
fn utf16_col_to_char_idx_ascii() {
    let line = "hello world";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0); // h
    assert_eq!(utf16_col_to_char_idx(line, 5), 5); // space
    assert_eq!(utf16_col_to_char_idx(line, 6), 6); // w
}

#[test]
fn utf16_col_to_char_idx_emoji() {
    // Emoji (😀) takes 2 UTF-16 code units but 1 char
    let line = "a😀b";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0); // a
    assert_eq!(utf16_col_to_char_idx(line, 1), 1); // 😀 (first UTF-16 unit)
    assert_eq!(utf16_col_to_char_idx(line, 2), 1); // 😀 (second UTF-16 unit, still same char)
    assert_eq!(utf16_col_to_char_idx(line, 3), 2); // b
}

#[test]
fn utf16_col_to_char_idx_chinese() {
    // Chinese char takes 1 UTF-16 code unit but 3 UTF-8 bytes
    let line = "a中b";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0); // a
    assert_eq!(utf16_col_to_char_idx(line, 1), 1); // 中
    assert_eq!(utf16_col_to_char_idx(line, 2), 2); // b
}

#[test]
fn utf16_col_past_end() {
    let line = "abc";
    assert_eq!(utf16_col_to_char_idx(line, 10), 3); // past end returns line length
}
