//! Parser + nav contracts (RED).

#![cfg(test)]

use super::nav::TableNav;
use super::parser::{MysqlTable, parse};

// ─── Parser ──────────────────────────────────────────────────────

const SIMPLE: &str = "\
+----+-------+
| id | name  |
+----+-------+
|  1 | alice |
|  2 | bob   |
+----+-------+
";

#[test]
fn parse_simple_two_column_table() {
    let t = parse(SIMPLE).expect("parsed");
    assert_eq!(t.headers, vec!["id", "name"]);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0], vec!["1", "alice"]);
    assert_eq!(t.rows[1], vec!["2", "bob"]);
}

#[test]
fn parse_trims_cell_padding() {
    // mysql pads cells with spaces for alignment — parser trims them.
    let t = parse(SIMPLE).expect("parsed");
    assert!(t.rows.iter().all(|r| r.iter().all(|c| !c.starts_with(' '))));
}

#[test]
fn parse_headers_only_table() {
    let s = "\
+----+
| id |
+----+
+----+
";
    let t = parse(s).expect("parsed");
    assert_eq!(t.headers, vec!["id"]);
    assert!(t.rows.is_empty());
}

#[test]
fn parse_handles_null_strings() {
    let s = "\
+----+-------+
| id | name  |
+----+-------+
|  1 | NULL  |
+----+-------+
";
    let t = parse(s).expect("parsed");
    assert_eq!(t.rows[0], vec!["1", "NULL"]);
}

#[test]
fn parse_preserves_internal_whitespace() {
    let s = "\
+----+-----------+
| id | name      |
+----+-----------+
|  1 | two words |
+----+-----------+
";
    let t = parse(s).expect("parsed");
    assert_eq!(t.rows[0], vec!["1", "two words"]);
}

#[test]
fn parse_handles_unicode_cells() {
    let s = "\
+----+------+
| id | 名字  |
+----+------+
|  1 | 爱丽丝 |
+----+------+
";
    let t = parse(s).expect("parsed");
    assert_eq!(t.headers.len(), 2);
    assert_eq!(t.rows[0].len(), 2);
    assert_eq!(t.rows[0][0], "1");
    assert_eq!(t.rows[0][1], "爱丽丝");
}

#[test]
fn parse_rejects_non_table_output() {
    assert!(parse("OK (no results)").is_none());
    assert!(parse("Error: something broke").is_none());
    assert!(parse("").is_none());
    assert!(parse("no borders just | pipes | text").is_none());
}

#[test]
fn parse_accepts_leading_trailing_whitespace() {
    let padded = format!("\n\n{SIMPLE}\n\n");
    assert!(parse(&padded).is_some());
}

#[test]
fn parse_three_column_wide_values() {
    let s = "\
+----+--------+--------------+
| id | name   | email        |
+----+--------+--------------+
|  1 | alice  | a@example.co |
|  2 | bob    | b@example.co |
|  3 | charlie| c@example.co |
+----+--------+--------------+
";
    let t = parse(s).expect("parsed");
    assert_eq!(t.num_cols(), 3);
    assert_eq!(t.num_rows(), 3);
    assert_eq!(t.rows[2][1], "charlie");
}

// ─── Nav ──────────────────────────────────────────────────────────

#[test]
fn nav_moves_down_and_clamps_at_last_row() {
    let mut n = TableNav::new(3, 4);
    n.move_down();
    assert_eq!(n.row, 1);
    n.move_down();
    assert_eq!(n.row, 2);
    n.move_down();
    assert_eq!(n.row, 2, "clamped at last");
}

#[test]
fn nav_moves_up_and_clamps_at_zero() {
    let mut n = TableNav::new(3, 4);
    n.row = 2;
    n.move_up();
    assert_eq!(n.row, 1);
    n.move_up();
    assert_eq!(n.row, 0);
    n.move_up();
    assert_eq!(n.row, 0, "clamped at first");
}

#[test]
fn nav_scroll_right_clamps_at_last_col() {
    let mut n = TableNav::new(3, 4);
    n.scroll_right();
    n.scroll_right();
    n.scroll_right();
    n.scroll_right();
    assert!(n.col_offset <= 3, "cannot scroll past last column");
}

#[test]
fn nav_scroll_left_clamps_at_zero() {
    let mut n = TableNav::new(3, 4);
    n.scroll_left();
    assert_eq!(n.col_offset, 0);
}

#[test]
fn nav_jump_start_end() {
    let mut n = TableNav::new(10, 4);
    n.jump_end();
    assert_eq!(n.row, 9);
    n.jump_start();
    assert_eq!(n.row, 0);
}

#[test]
fn nav_on_empty_table_is_noop() {
    let mut n = TableNav::new(0, 0);
    n.move_down();
    n.move_up();
    n.scroll_right();
    assert!(!n.row_valid());
}

// Smoke check
#[test]
fn mysql_table_methods() {
    let t = MysqlTable {
        headers: vec!["a".into(), "b".into()],
        rows: vec![vec!["1".into(), "2".into()]],
    };
    assert!(!t.is_empty());
    assert_eq!(t.num_cols(), 2);
    assert_eq!(t.num_rows(), 1);
}
