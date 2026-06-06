use crate::cli::session::session_state::SessionState;
use crate::{cli_dim, cli_info, cli_section};

/// Handle `/sync` without direct DB access.
///
/// Cloud sync orchestration is server-owned; the CLI no longer opens MatrixOne
/// pools or drives sync domains directly.
pub(crate) async fn handle_sync_command(arg: &str, _state: &SessionState) {
    let sub = arg.trim();
    cli_section!("Sync Engine Status");
    eprintln!();
    match sub {
        "log" => {
            cli_info!(
                "/sync {sub} is server-owned; use the API/server diagnostics for cloud sync state."
            );
        }
        "" => {
            cli_dim!("Cloud sync is managed by the astra-server API.");
        }
        _ => {
            cli_info!("Usage: /sync [log]");
            cli_dim!("Cloud sync commands no longer connect to MatrixOne from the CLI.");
        }
    }
    eprintln!();
}
