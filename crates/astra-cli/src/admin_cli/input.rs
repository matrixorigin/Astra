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
