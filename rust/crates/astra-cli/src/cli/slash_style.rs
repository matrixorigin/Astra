#![allow(unused_imports)]
use super::*;

pub(super) fn handle_style_command(arg: &str) {
    match arg {
        "" | "list" => {
            let current = theme::current_theme_name();
            eprintln!(
                "\n{}",
                "─── Output Styles ───────────────────────────────".bold()
            );
            eprintln!("  {}\n", "Built-in:".dim());
            for t in theme::builtin_themes() {
                let marker = if t.name == current { " ◉" } else { "  " };
                let name = &t.name;
                eprintln!("  {marker} {}", name.as_str().cyan());
            }
            let user_themes = theme::load_user_themes();
            if !user_themes.is_empty() {
                eprintln!("\n  {}\n", "User (~/.astra/styles/):".dim());
                for t in &user_themes {
                    let marker = if t.name == current { " ◉" } else { "  " };
                    let name = &t.name;
                    eprintln!("  {marker} {}", name.as_str().cyan());
                }
            }
            eprintln!(
                "\n  {}",
                "Use /style <name> to switch. Active theme marked with ◉.".dim()
            );
            eprintln!();
        }
        name => match theme::activate_theme_by_name(name) {
            Ok(()) => {
                eprintln!(
                    "  {} {}",
                    theme::icon_ok(),
                    format!("Style set to: {name}").green()
                );
            }
            Err(e) => {
                eprintln!("  {} {e}", theme::icon_err());
                let available: Vec<_> = theme::builtin_themes()
                    .iter()
                    .map(|t| t.name.clone())
                    .chain(theme::load_user_themes().iter().map(|t| t.name.clone()))
                    .collect();
                eprintln!(
                    "  {} Available: {}",
                    theme::icon_info(),
                    available.join(", ")
                );
            }
        },
    }
}
