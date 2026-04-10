#![allow(unused_imports)]
use crate::{cli_dim, cli_err, cli_info, cli_ok, cli_section};
use super::*;

pub(super) fn handle_style_command(arg: &str) {
    match arg {
        "" | "list" => {
            let current = theme::current_theme_name();
            cli_section!("Output Styles");
            eprintln!();
            cli_dim!("Built-in:");
            eprintln!();
            for t in theme::builtin_themes() {
                let marker = if t.name == current { " ◉" } else { "  " };
                let name = &t.name;
                eprintln!("  {marker} {}", name.as_str().cyan());
            }
            let user_themes = theme::load_user_themes();
            if !user_themes.is_empty() {
                eprintln!();
                cli_dim!("User (~/.astra/styles/):");
                eprintln!();
                for t in &user_themes {
                    let marker = if t.name == current { " ◉" } else { "  " };
                    let name = &t.name;
                    eprintln!("  {marker} {}", name.as_str().cyan());
                }
            }
            eprintln!();
            cli_dim!("Use /style <name> to switch. Active theme marked with ◉.");
            eprintln!();
        }
        name => match theme::activate_theme_by_name(name) {
            Ok(()) => {
                cli_ok!("Style set to: {}", name);
            }
            Err(e) => {
                cli_err!("{}", e);
                let available: Vec<_> = theme::builtin_themes()
                    .iter()
                    .map(|t| t.name.clone())
                    .chain(theme::load_user_themes().iter().map(|t| t.name.clone()))
                    .collect();
                cli_info!("Available: {}", available.join(", "));
            }
        },
    }
}
