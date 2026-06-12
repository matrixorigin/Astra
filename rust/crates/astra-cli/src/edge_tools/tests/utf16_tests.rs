use super::utf16_col_to_char_idx;

#[test]
fn utf16_col_to_char_idx_conversion() {
    // ASCII: 1:1 mapping
    let line = "hello world";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0);
    assert_eq!(utf16_col_to_char_idx(line, 5), 5);
    assert_eq!(utf16_col_to_char_idx(line, 6), 6);

    // Emoji (😀) takes 2 UTF-16 code units but 1 char
    let line = "a😀b";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0); // a
    assert_eq!(utf16_col_to_char_idx(line, 1), 1); // 😀 first unit
    assert_eq!(utf16_col_to_char_idx(line, 2), 1); // 😀 second unit, same char
    assert_eq!(utf16_col_to_char_idx(line, 3), 2); // b

    // Chinese char: 1 UTF-16 unit, 3 UTF-8 bytes
    let line = "a中b";
    assert_eq!(utf16_col_to_char_idx(line, 0), 0); // a
    assert_eq!(utf16_col_to_char_idx(line, 1), 1); // 中
    assert_eq!(utf16_col_to_char_idx(line, 2), 2); // b

    // Past end returns line length
    assert_eq!(utf16_col_to_char_idx("abc", 10), 3);
}
