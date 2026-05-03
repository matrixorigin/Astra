//! Markdown rewriting for WeChat (mobile-friendly).
//!
//! WeChat doesn't render markdown natively. Convert:
//! - `# H1` → `【H1】`, `## H2+` → `**H2**`
//! - Tables → description lists
//! - Keep code fences, bold, italic, links as-is (WeChat shows them ok-ish)

pub fn rewrite_for_weixin(text: &str) -> String {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    let mut in_code_block = false;

    while let Some(line) = lines.next() {
        // Track code fences
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            out.push(line.to_string());
            continue;
        }
        if in_code_block {
            out.push(line.to_string());
            continue;
        }

        // Rewrite headers
        if let Some(rewritten) = rewrite_header(line) {
            out.push(rewritten);
            continue;
        }

        // Detect table block
        if is_table_row(line) {
            let mut table_lines = vec![line.to_string()];
            while let Some(next) = lines.peek() {
                if is_table_row(next) || is_separator_row(next) {
                    table_lines.push(lines.next().unwrap().to_string());
                } else {
                    break;
                }
            }
            out.push(rewrite_table(&table_lines));
            continue;
        }

        out.push(line.to_string());
    }

    out.join("\n")
}

fn rewrite_header(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || !trimmed[level..].starts_with(' ') {
        return None;
    }
    let title = trimmed[level..].trim();
    if title.is_empty() {
        return None;
    }
    if level == 1 {
        Some(format!("【{title}】"))
    } else {
        Some(format!("**{title}**"))
    }
}

fn is_table_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('|') && t.len() > 2
}

fn is_separator_row(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|')
        && t.chars()
            .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

fn split_table_row(line: &str) -> Vec<String> {
    let mut row = line.trim();
    if row.starts_with('|') {
        row = &row[1..];
    }
    if row.ends_with('|') {
        row = &row[..row.len() - 1];
    }
    row.split('|').map(|c| c.trim().to_string()).collect()
}

fn rewrite_table(lines: &[String]) -> String {
    if lines.len() < 2 {
        return lines.join("\n");
    }
    let headers = split_table_row(&lines[0]);
    // Skip separator row (line 1), process data rows
    let data_start = if lines.len() > 1 && is_separator_row(&lines[1]) {
        2
    } else {
        1
    };
    let mut formatted = Vec::new();

    for line in &lines[data_start..] {
        if is_separator_row(line) {
            continue;
        }
        let cells = split_table_row(line);
        let pairs: Vec<_> = headers
            .iter()
            .zip(cells.iter())
            .filter(|(_, v)| !v.is_empty())
            .collect();

        if pairs.is_empty() {
            continue;
        }
        if pairs.len() <= 2 {
            for (label, value) in &pairs {
                formatted.push(format!("- {label}: {value}"));
            }
        } else {
            let summary: String = pairs
                .iter()
                .map(|(l, v)| format!("{l}: {v}"))
                .collect::<Vec<_>>()
                .join(" | ");
            formatted.push(format!("- {summary}"));
        }
    }

    if formatted.is_empty() {
        lines.join("\n")
    } else {
        formatted.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_h1() {
        assert_eq!(rewrite_for_weixin("# Title"), "【Title】");
    }

    #[test]
    fn header_h2() {
        assert_eq!(rewrite_for_weixin("## Subtitle"), "**Subtitle**");
    }

    #[test]
    fn header_h3() {
        assert_eq!(rewrite_for_weixin("### Section"), "**Section**");
    }

    #[test]
    fn header_inside_code_block_preserved() {
        let input = "```\n# not a header\n```";
        assert_eq!(rewrite_for_weixin(input), input);
    }

    #[test]
    fn simple_table() {
        let input = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |";
        let result = rewrite_for_weixin(input);
        assert!(result.contains("- Name: Alice"), "got: {result}");
        assert!(result.contains("- Age: 30"), "got: {result}");
        assert!(result.contains("- Name: Bob"), "got: {result}");
        assert!(!result.contains("|---"), "separator should be removed");
    }

    #[test]
    fn wide_table_collapses_to_summary() {
        let input = "| A | B | C | D |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |";
        let result = rewrite_for_weixin(input);
        assert!(result.contains("A: 1"), "got: {result}");
        assert!(result.contains("B: 2"), "got: {result}");
    }

    #[test]
    fn no_table_passthrough() {
        let input = "just text\nmore text";
        assert_eq!(rewrite_for_weixin(input), input);
    }

    #[test]
    fn mixed_content() {
        let input =
            "# Report\n\nSome text.\n\n| Key | Value |\n|-----|-------|\n| foo | bar |\n\nDone.";
        let result = rewrite_for_weixin(input);
        assert!(result.contains("【Report】"));
        assert!(result.contains("Some text."));
        assert!(result.contains("- Key: foo"));
        assert!(result.contains("Done."));
    }

    #[test]
    fn code_block_preserved() {
        let input = "```rust\nfn main() {}\n```";
        assert_eq!(rewrite_for_weixin(input), input);
    }

    #[test]
    fn empty_input() {
        assert_eq!(rewrite_for_weixin(""), "");
    }

    #[test]
    fn bold_italic_preserved() {
        let input = "This is **bold** and *italic*.";
        assert_eq!(rewrite_for_weixin(input), input);
    }

    #[test]
    fn header_no_space_after_hash() {
        // "##NoSpace" is not a valid header
        assert_eq!(rewrite_header("##NoSpace"), None);
    }
}
