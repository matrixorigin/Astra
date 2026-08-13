use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedArtifactFileMetadata {
    pub content_type: String,
    pub artifact_kind: String,
}

pub fn validate_short_token(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    if value.len() > max_len {
        return Err(format!("Error: {field} must be at most {max_len} bytes"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!(
            "Error: {field} must not contain control characters"
        ));
    }
    Ok(())
}

pub fn validate_artifact_kind(value: &str) -> Result<String, String> {
    validate_short_token(value, "artifact_kind", 64)?;
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(
            "Error: artifact_kind may only contain ASCII letters, digits, '_', '-', or '.'"
                .to_string(),
        );
    }
    Ok(value.to_ascii_lowercase())
}

pub fn validate_content_type(value: &str) -> Result<String, String> {
    validate_short_token(value, "content_type", 128)?;
    if !value.contains('/') || value.contains(';') {
        return Err("Error: content_type must be a simple MIME type such as image/png".to_string());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn infer_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("pdf") => "application/pdf",
        Some("html") | Some("htm") => "text/html",
        Some("md") | Some("markdown") => "text/markdown",
        Some("txt") | Some("log") => "text/plain",
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("parquet") => "application/vnd.apache.parquet",
        _ => "application/octet-stream",
    }
}

pub fn infer_artifact_kind(path: &Path, content_type: &str) -> &'static str {
    if content_type.starts_with("image/") {
        return "image";
    }
    match content_type {
        "application/pdf" => "pdf",
        "text/html" => "html",
        "text/markdown" => "markdown",
        "application/json"
        | "application/x-ndjson"
        | "text/csv"
        | "text/tab-separated-values"
        | "application/yaml"
        | "application/toml" => "data",
        "text/plain" => "text",
        "application/zip" | "application/x-tar" | "application/gzip" => "archive",
        _ => match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "sql" | "sh") => "code",
            _ => "file",
        },
    }
}

pub fn normalize_artifact_file_metadata(
    path: &Path,
    content_type: Option<&str>,
    artifact_kind: Option<&str>,
) -> Result<NormalizedArtifactFileMetadata, String> {
    let content_type = match content_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => validate_content_type(value)?,
        None => infer_content_type(path).to_string(),
    };
    let artifact_kind = match artifact_kind
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => validate_artifact_kind(value)?,
        None => infer_artifact_kind(path, &content_type).to_string(),
    };
    Ok(NormalizedArtifactFileMetadata {
        content_type,
        artifact_kind,
    })
}

pub fn should_store_artifact_as_text(content_type: &str, path: &Path) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "image/svg+xml"
                | "application/json"
                | "application/x-ndjson"
                | "application/yaml"
                | "application/toml"
                | "application/xml"
        )
        || matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "sql" | "sh")
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_validates_overrides_and_infers_common_files() {
        assert_eq!(
            normalize_artifact_file_metadata(Path::new("report.pdf"), None, None).unwrap(),
            NormalizedArtifactFileMetadata {
                content_type: "application/pdf".to_string(),
                artifact_kind: "pdf".to_string(),
            }
        );
        assert_eq!(
            normalize_artifact_file_metadata(
                Path::new("plot.bin"),
                Some(" Image/PNG "),
                Some(" Report.Image "),
            )
            .unwrap(),
            NormalizedArtifactFileMetadata {
                content_type: "image/png".to_string(),
                artifact_kind: "report.image".to_string(),
            }
        );
        assert!(
            normalize_artifact_file_metadata(
                Path::new("report.txt"),
                Some("text/plain; charset=utf-8"),
                None,
            )
            .is_err()
        );
    }
}
