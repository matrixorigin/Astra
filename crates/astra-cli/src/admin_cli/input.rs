pub(crate) fn prompt_or(label: &str, existing: Option<String>) -> Result<String, String> {
    if let Some(v) = existing {
        return Ok(v);
    }
    use std::io;
    stdout_print!("{}: ", label);
    match crate::cli::stream::output_sink::flush_stdout().map_err(|e| e.to_string())? {
        crate::cli::stream::output_sink::OutputWriteStatus::Written => {}
        crate::cli::stream::output_sink::OutputWriteStatus::Closed => {
            return Err("stdout output transport closed by its consumer".to_string());
        }
    }
    let mut val = String::new();
    io::stdin().read_line(&mut val).map_err(|e| e.to_string())?;
    let val = val.trim().to_string();
    if val.is_empty() {
        Err(format!("{label} cannot be empty"))
    } else {
        Ok(val)
    }
}

/// Read a secret without echoing it to the terminal or shell history.
pub(crate) fn prompt_secret(label: &str) -> Result<String, String> {
    let value = rpassword::prompt_password(format!("{label}: ")).map_err(|e| e.to_string())?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(format!("{label} cannot be empty"))
    } else {
        Ok(value)
    }
}

pub(crate) fn prompt_secret_or(label: &str, existing: Option<String>) -> Result<String, String> {
    existing.map_or_else(|| prompt_secret(label), Ok)
}
