use serde_json::Value;
use thiserror::Error;

/// Failure to replace an existing value addressed by a dotted JSON path.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JsonPathMutationError {
    #[error("mutation path cannot be empty")]
    EmptyPath,
    #[error("mutation path contains an empty segment: '{path}'")]
    EmptySegment { path: String },
    #[error("unknown config path segment '{segment}'")]
    UnknownSegment { segment: String },
    #[error("config path '{path}' does not point to an object parent")]
    ParentNotObject { path: String },
    #[error("unknown config leaf '{leaf}'")]
    UnknownLeaf { leaf: String },
}

fn locate_existing_json_path<'a, 'p>(
    root: &'a Value,
    path: &'p str,
) -> Result<(&'a Value, Vec<&'p str>), JsonPathMutationError> {
    if path.is_empty() {
        return Err(JsonPathMutationError::EmptyPath);
    }
    let segments = path.split('.').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(JsonPathMutationError::EmptySegment {
            path: path.to_string(),
        });
    }

    let mut current = root;
    for (index, segment) in segments.iter().enumerate() {
        let object = current
            .as_object()
            .ok_or_else(|| JsonPathMutationError::ParentNotObject {
                path: path.to_string(),
            })?;
        current = object.get(*segment).ok_or_else(|| {
            if index + 1 == segments.len() {
                JsonPathMutationError::UnknownLeaf {
                    leaf: (*segment).to_string(),
                }
            } else {
                JsonPathMutationError::UnknownSegment {
                    segment: (*segment).to_string(),
                }
            }
        })?;
    }
    Ok((current, segments))
}

/// Read one existing value addressed by the canonical dotted-path grammar.
pub fn read_existing_json_path(root: &Value, path: &str) -> Result<Value, JsonPathMutationError> {
    locate_existing_json_path(root, path).map(|(value, _)| value.clone())
}

/// Replace one existing leaf in a JSON object and return its previous value.
///
/// The path grammar is a non-empty sequence of non-empty keys separated by
/// dots. This function never creates objects or leaves. Failed mutations leave
/// `root` unchanged.
pub fn replace_existing_json_path(
    root: &mut Value,
    path: &str,
    new_value: Value,
) -> Result<Value, JsonPathMutationError> {
    let (old, segments) = locate_existing_json_path(root, path)?;
    let old = old.clone();
    let (leaf, parents) = segments.split_last().expect("validated non-empty path");
    let mut current = root;
    for segment in parents {
        current = current
            .get_mut(*segment)
            .expect("path was validated against the same JSON value");
    }
    let slot = current
        .get_mut(*leaf)
        .expect("leaf was validated against the same JSON value");
    *slot = new_value;
    Ok(old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_existing_leaf_and_returns_previous_value() {
        let mut root = json!({"memory": {"retrieval_top_k": 5}});
        assert_eq!(
            read_existing_json_path(&root, "memory.retrieval_top_k").unwrap(),
            json!(5)
        );

        let old = replace_existing_json_path(&mut root, "memory.retrieval_top_k", json!(8))
            .expect("existing leaf");

        assert_eq!(old, json!(5));
        assert_eq!(root["memory"]["retrieval_top_k"], json!(8));
    }

    #[test]
    fn failures_are_typed_and_atomic() {
        let original = json!({"memory": {"retrieval_top_k": 5}});
        let cases = [
            ("", JsonPathMutationError::EmptyPath),
            (
                "memory..retrieval_top_k",
                JsonPathMutationError::EmptySegment {
                    path: "memory..retrieval_top_k".to_string(),
                },
            ),
            (
                "memory.missing.value",
                JsonPathMutationError::UnknownSegment {
                    segment: "missing".to_string(),
                },
            ),
            (
                "memory.retrieval_top_k.value",
                JsonPathMutationError::ParentNotObject {
                    path: "memory.retrieval_top_k.value".to_string(),
                },
            ),
            (
                "memory.retrieval_top_k.value.deeper",
                JsonPathMutationError::ParentNotObject {
                    path: "memory.retrieval_top_k.value.deeper".to_string(),
                },
            ),
            (
                "memory.missing",
                JsonPathMutationError::UnknownLeaf {
                    leaf: "missing".to_string(),
                },
            ),
        ];

        for (path, expected) in cases {
            let mut root = original.clone();
            let error = replace_existing_json_path(&mut root, path, json!(8))
                .expect_err("invalid path must fail");
            assert_eq!(error, expected, "path={path}");
            assert_eq!(root, original, "failed mutation must be atomic: {path}");
            assert_eq!(read_existing_json_path(&root, path).unwrap_err(), expected);
        }
    }
}
