//! Parse mysql-client ASCII table output — RED phase stub.

#![allow(dead_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MysqlTable {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl MysqlTable {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn num_cols(&self) -> usize {
        self.headers.len()
    }

    pub fn num_rows(&self) -> usize {
        self.rows.len()
    }
}

/// Attempt to parse mysql `-e` client output (ASCII table) into a
/// structured [`MysqlTable`]. Returns `None` when the input doesn't
/// look like a table (e.g. "OK (no results)" or error text).
///
/// Expected shape:
///
/// ```text
/// +----+-------+
/// | id | name  |
/// +----+-------+
/// |  1 | alice |
/// +----+-------+
/// ```
pub(crate) fn parse(output: &str) -> Option<MysqlTable> {
    let lines: Vec<&str> = output.lines().map(str::trim_end).collect();

    // Skip leading blank lines.
    let mut i = 0;
    while i < lines.len() && lines[i].trim().is_empty() {
        i += 1;
    }
    if i + 2 >= lines.len() {
        return None;
    }

    // First non-blank line must be a border.
    if !is_border(lines[i]) {
        return None;
    }
    let header_idx = i + 1;
    if !is_data_row(lines[header_idx]) {
        return None;
    }
    let mid_border_idx = i + 2;
    if !is_border(lines[mid_border_idx]) {
        return None;
    }

    let headers = split_row(lines[header_idx]);
    if headers.is_empty() {
        return None;
    }

    let mut rows = Vec::new();
    let mut j = mid_border_idx + 1;
    while j < lines.len() {
        let l = lines[j];
        if l.trim().is_empty() {
            break;
        }
        if is_border(l) {
            break; // closing border
        }
        if !is_data_row(l) {
            return None;
        }
        let cells = split_row(l);
        if cells.len() != headers.len() {
            return None;
        }
        rows.push(cells);
        j += 1;
    }

    Some(MysqlTable { headers, rows })
}

fn is_border(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.starts_with('+')
        && t.ends_with('+')
        && t.chars().all(|c| c == '+' || c == '-')
}

fn is_data_row(s: &str) -> bool {
    let t = s.trim();
    t.starts_with('|') && t.ends_with('|')
}

/// Split a `| cell | cell |` row into trimmed cell values.
fn split_row(s: &str) -> Vec<String> {
    let t = s.trim();
    // Strip the leading and trailing `|` then split on interior pipes.
    let inner = &t[1..t.len() - 1];
    inner
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}
