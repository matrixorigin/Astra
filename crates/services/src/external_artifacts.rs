use serde_json::{Map, Value};

const ID_MAX_BYTES: usize = 255;
const KIND_MAX_BYTES: usize = 64;
const TITLE_MAX_BYTES: usize = 160;
const FILENAME_MAX_BYTES: usize = 255;
const DESCRIPTION_MAX_BYTES: usize = 1_000;
const CONTENT_TYPE_MAX_BYTES: usize = 128;
const TOKEN_MAX_BYTES: usize = 128;

/// Project untrusted runtime artifact descriptors onto the bounded external
/// contract shared by durable replay and live terminal events.
pub fn project_external_artifacts(value: &Value) -> Option<Value> {
    let artifacts = value
        .as_array()?
        .iter()
        .filter_map(project_external_artifact)
        .collect::<Vec<_>>();
    (!artifacts.is_empty()).then(|| Value::Array(artifacts))
}

fn project_external_artifact(value: &Value) -> Option<Value> {
    let mut artifact = value.as_object()?.clone();
    sanitize_short_fields(&mut artifact, &["artifact_id", "id"], ID_MAX_BYTES);
    sanitize_artifact_kind_fields(&mut artifact, &["type", "kind"]);
    if !has_non_empty_string(&artifact, &["artifact_id", "id"])
        || !has_non_empty_string(&artifact, &["type", "kind"])
    {
        return None;
    }
    sanitize_short_fields(&mut artifact, &["name", "title"], TITLE_MAX_BYTES);
    sanitize_short_fields(
        &mut artifact,
        &["filename", "download_filename"],
        FILENAME_MAX_BYTES,
    );
    sanitize_content_type_fields(&mut artifact, &["content_type", "mime_type"]);
    sanitize_artifact_kind_fields(&mut artifact, &["artifact_kind"]);
    sanitize_short_fields(&mut artifact, &["description"], DESCRIPTION_MAX_BYTES);
    sanitize_short_fields(
        &mut artifact,
        &["created_at", "renderer", "source"],
        TOKEN_MAX_BYTES,
    );

    if let Some(data) = artifact.get_mut("data").and_then(Value::as_object_mut) {
        sanitize_short_fields(
            data,
            &["name", "filename", "download_filename"],
            FILENAME_MAX_BYTES,
        );
        sanitize_content_type_fields(data, &["content_type", "mime_type"]);
        sanitize_short_fields(data, &["file_id", "renderer", "encoding"], TOKEN_MAX_BYTES);
    }
    if let Some(metadata) = artifact.get_mut("metadata").and_then(Value::as_object_mut) {
        sanitize_short_fields(
            metadata,
            &["filename", "download_filename"],
            FILENAME_MAX_BYTES,
        );
        sanitize_content_type_fields(metadata, &["content_type", "mime_type"]);
        sanitize_artifact_kind_fields(metadata, &["artifact_kind"]);
        sanitize_short_fields(
            metadata,
            &["source", "tool_id", "file_id", "sha256", "renderer"],
            TOKEN_MAX_BYTES,
        );
    }
    if let Some(parts) = artifact.get_mut("parts").and_then(Value::as_array_mut) {
        for part in parts {
            let Some(file) = part
                .as_object_mut()
                .and_then(|part| part.get_mut("file"))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            sanitize_short_fields(file, &["name"], FILENAME_MAX_BYTES);
            sanitize_content_type_fields(file, &["mimeType"]);
        }
    }
    Some(Value::Object(artifact))
}

fn has_non_empty_string(object: &Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    })
}

fn sanitize_short_fields(object: &mut Map<String, Value>, keys: &[&str], max_bytes: usize) {
    for key in keys {
        let valid = object
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| valid_short_string(value, max_bytes));
        if object.contains_key(*key) && !valid {
            object.remove(*key);
        }
    }
}

fn sanitize_artifact_kind_fields(object: &mut Map<String, Value>, keys: &[&str]) {
    sanitize_normalized_string_fields(object, keys, valid_artifact_kind);
}

fn sanitize_content_type_fields(object: &mut Map<String, Value>, keys: &[&str]) {
    sanitize_normalized_string_fields(object, keys, valid_content_type);
}

fn sanitize_normalized_string_fields(
    object: &mut Map<String, Value>,
    keys: &[&str],
    validate: fn(&str) -> bool,
) {
    for key in keys {
        let normalized = object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| validate(value))
            .map(str::to_ascii_lowercase);
        match normalized {
            Some(value) => {
                object.insert((*key).to_string(), Value::String(value));
            }
            None => {
                object.remove(*key);
            }
        }
    }
}

fn valid_short_string(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

fn valid_artifact_kind(value: &str) -> bool {
    valid_short_string(value, KIND_MAX_BYTES)
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn valid_content_type(value: &str) -> bool {
    if !valid_short_string(value, CONTENT_TYPE_MAX_BYTES) || value.contains(';') {
        return false;
    }
    let Some((media_type, subtype)) = value.split_once('/') else {
        return false;
    };
    !media_type.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && media_type.chars().all(valid_mime_token_char)
        && subtype.chars().all(valid_mime_token_char)
}

fn valid_mime_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn projection_sanitizes_metadata_and_drops_invalid_descriptors() {
        let projected = project_external_artifacts(&json!([
            {
                "artifact_id": "file-1",
                "name": "x".repeat(TITLE_MAX_BYTES + 1),
                "description": "y".repeat(DESCRIPTION_MAX_BYTES + 1),
                "type": "FILE",
                "content_type": "not-a-mime",
                "artifact_kind": "REPORT.FILE",
                "data": {
                    "filename": "z".repeat(FILENAME_MAX_BYTES + 1),
                    "content_type": "text/html; charset=utf-8",
                    "mime_type": "IMAGE/PNG"
                },
                "metadata": {
                    "artifact_kind": "Report.Image",
                    "content_type": "not-a-mime"
                },
                "parts": [{
                    "file": {
                        "name": "bad\nname.pdf",
                        "mimeType": "APPLICATION/PDF"
                    }
                }]
            },
            {
                "artifact_id": "i".repeat(ID_MAX_BYTES + 1),
                "type": "file"
            },
            {
                "artifact_id": "file-3",
                "type": "../../file"
            }
        ]))
        .unwrap();

        let artifacts = projected.as_array().unwrap();
        assert_eq!(artifacts.len(), 1);
        let artifact = &artifacts[0];
        assert_eq!(artifact["artifact_id"], "file-1");
        assert_eq!(artifact["type"], "file");
        assert!(artifact.get("name").is_none());
        assert!(artifact.get("description").is_none());
        assert!(artifact.get("content_type").is_none());
        assert_eq!(artifact["artifact_kind"], "report.file");
        assert!(artifact["data"].get("filename").is_none());
        assert!(artifact["data"].get("content_type").is_none());
        assert_eq!(artifact["data"]["mime_type"], "image/png");
        assert_eq!(artifact["metadata"]["artifact_kind"], "report.image");
        assert!(artifact["metadata"].get("content_type").is_none());
        assert!(artifact["parts"][0]["file"].get("name").is_none());
        assert_eq!(artifact["parts"][0]["file"]["mimeType"], "application/pdf");
    }
}
