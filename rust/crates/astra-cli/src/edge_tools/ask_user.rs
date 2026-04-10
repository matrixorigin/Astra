//! Interactive ask_user tool implementation with crossterm raw mode.
//!
//! Supports multiple choice (single-key selection) and free-form text input
//! with timeout, length limits, and keyboard navigation.

use crossterm::style::Stylize;
use serde_json::Value;

use super::ToolExecutor;

impl ToolExecutor {
    pub(super) fn ask_user(&self, args: &Value) -> String {
        use crossterm::{
            event::{self, Event, KeyCode, KeyEvent},
            terminal::{disable_raw_mode, enable_raw_mode},
        };
        use std::io::{self, Write};
        use std::time::Duration;

        const MAX_INPUT_LEN: usize = 4096; // 4KB limit

        let question = match args.get("question").and_then(Value::as_str) {
            Some(q) if !q.is_empty() => q,
            _ => return "Error: 'question' is required".to_string(),
        };

        let choices: Vec<&str> = args
            .get("choices")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Validate choices count (2-9 for single-key optimization)
        if !choices.is_empty() && (choices.len() < 2 || choices.len() > 9) {
            return "Error: choices must contain 2-9 options".to_string();
        }

        let default = args.get("default").and_then(Value::as_str);
        let context = args.get("context").and_then(Value::as_str);

        // Display the question
        eprintln!();
        if let Some(ctx) = context {
            eprintln!("  {}", ctx.dim());
        }
        eprintln!("  {} {}", "▸".cyan(), question.bold().cyan());

        if choices.is_empty() {
            // Free-form input
            eprintln!();
            let prompt = if let Some(def) = default {
                format!("  {} {} ", format!("[{def}]").dim(), "→".cyan())
            } else {
                format!("  {} ", "→".cyan())
            };
            eprint!("{}", prompt);
            let _ = io::stderr().flush();

            let mut response = String::new();
            if io::stdin().read_line(&mut response).is_err() {
                return "Error: failed to read user input".to_string();
            }
            // Truncate if too long
            if response.len() > MAX_INPUT_LEN {
                response.truncate(MAX_INPUT_LEN);
            }
            let response = response.trim();
            let answer = if response.is_empty() {
                default.unwrap_or("").to_string()
            } else {
                response.to_string()
            };
            serde_json::json!({
                "answer": answer,
                "question": question
            })
            .to_string()
        } else {
            // Multiple choice
            eprintln!();
            for (i, choice) in choices.iter().enumerate() {
                let num = i + 1;
                let is_default = default.map_or(i == 0, |d| *choice == d);
                if is_default {
                    eprintln!(
                        "  {} {} {}",
                        "▸".cyan(),
                        format!("[{num}]").cyan(),
                        choice.bold()
                    );
                } else {
                    eprintln!("    {} {}", format!("[{num}]").dim(), choice.dim());
                }
            }
            eprintln!();
            eprint!("  {} ", "→".cyan());
            let _ = io::stderr().flush();

            // Try raw mode for single-key selection
            struct RawModeGuard;
            impl Drop for RawModeGuard {
                fn drop(&mut self) {
                    let _ = disable_raw_mode();
                }
            }

            let answer = if enable_raw_mode().is_ok() {
                let _guard = RawModeGuard;
                let mut input = String::new();
                let mut consecutive_errors = 0u8;
                loop {
                    // Use poll with timeout to avoid infinite spin on persistent errors
                    match event::poll(Duration::from_millis(100)) {
                        Ok(true) => {
                            match event::read() {
                                Ok(Event::Key(KeyEvent { code, .. })) => {
                                    consecutive_errors = 0;
                                    match code {
                                        KeyCode::Char(c)
                                            if c.is_ascii_digit() && input.is_empty() =>
                                        {
                                            let idx = c.to_digit(10).expect("ascii digit") as usize;
                                            if idx >= 1 && idx <= choices.len() {
                                                drop(_guard);
                                                eprintln!("{}", c);
                                                break choices[idx - 1].to_string();
                                            }
                                            input.push(c);
                                            eprint!("{}", c);
                                        }
                                        KeyCode::Char(c) => {
                                            if input.len() < MAX_INPUT_LEN {
                                                input.push(c);
                                                eprint!("{}", c);
                                            }
                                        }
                                        KeyCode::Backspace if !input.is_empty() => {
                                            input.pop();
                                            eprint!("\x08 \x08");
                                        }
                                        KeyCode::Enter => {
                                            drop(_guard);
                                            eprintln!();
                                            let trimmed = input.trim();
                                            if trimmed.is_empty() {
                                                break default.unwrap_or(choices[0]).to_string();
                                            }
                                            if let Ok(idx) = trimmed.parse::<usize>() {
                                                if idx >= 1 && idx <= choices.len() {
                                                    break choices[idx - 1].to_string();
                                                }
                                            }
                                            break trimmed.to_string();
                                        }
                                        KeyCode::Esc => {
                                            drop(_guard);
                                            eprintln!();
                                            break "[cancelled]".to_string();
                                        }
                                        _ => {}
                                    }
                                }
                                Ok(_) => {} // Ignore non-key events
                                Err(_) => {
                                    consecutive_errors += 1;
                                    if consecutive_errors >= 5 {
                                        drop(_guard);
                                        eprintln!();
                                        break "[error: terminal read failed]".to_string();
                                    }
                                }
                            }
                        }
                        Ok(false) => continue, // Timeout, poll again
                        Err(_) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= 5 {
                                drop(_guard);
                                eprintln!();
                                break "[error: terminal unavailable]".to_string();
                            }
                        }
                    }
                    let _ = io::stderr().flush();
                }
            } else {
                // Fallback: line-based input
                let mut response = String::new();
                if io::stdin().read_line(&mut response).is_err() {
                    return "Error: failed to read user input".to_string();
                }
                if response.len() > MAX_INPUT_LEN {
                    response.truncate(MAX_INPUT_LEN);
                }
                let trimmed = response.trim();
                if trimmed.is_empty() {
                    default.unwrap_or(choices[0]).to_string()
                } else if let Ok(idx) = trimmed.parse::<usize>() {
                    if idx >= 1 && idx <= choices.len() {
                        choices[idx - 1].to_string()
                    } else {
                        trimmed.to_string()
                    }
                } else {
                    trimmed.to_string()
                }
            };

            serde_json::json!({
                "answer": answer,
                "question": question,
                "was_custom": !choices.contains(&answer.as_str())
            })
            .to_string()
        }
    }
}
