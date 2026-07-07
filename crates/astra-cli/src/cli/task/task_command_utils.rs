pub(crate) fn task_run_title(prompt: &str) -> String {
    let mut title = String::with_capacity("run: ".len() + prompt.len().min(63));
    title.push_str("run: ");
    match prompt.char_indices().nth(60) {
        Some((idx, _)) => {
            title.push_str(&prompt[..idx]);
            title.push_str("...");
        }
        None => title.push_str(prompt),
    }
    title
}

#[cfg(test)]
mod tests {
    use super::task_run_title;

    #[test]
    fn task_run_title_keeps_short_prompt() {
        assert_eq!(task_run_title("build auth"), "run: build auth");
    }

    #[test]
    fn task_run_title_truncates_long_prompt_by_chars() {
        let prompt = "a".repeat(61);
        let title = task_run_title(&prompt);
        assert_eq!(title, format!("run: {}...", "a".repeat(60)));
    }
}
