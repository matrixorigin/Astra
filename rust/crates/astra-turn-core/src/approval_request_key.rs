//! Issue #326 P1.5: strict request identity for queue dedup + stale revalidation.
//!
//! `ApprovalRequestKey` answers "are these two pending tool calls the *same
//! request*?". It exists separately from
//! [`crate::approval_fingerprint::ApprovalFingerprint`] because the two
//! questions need OPPOSITE accuracy:
//!
//! - **Rule matching** (the existing fingerprint) wants to be GENEROUS.
//!   A user-saved rule like `Bash(npm test:*)` should match any future
//!   `npm test` call regardless of arguments. The fingerprint normalizes
//!   path patterns (`approval_fingerprint.rs:185-197`) and tolerates wide
//!   command matches (`approval_fingerprint.rs:99-130`). Generous = good.
//!
//! - **Queue dedup** must be PRECISE. Two `edit_file` calls touching
//!   different files are NOT the same request, even though they share a
//!   tool name + side-effect class. Merging them into one approval
//!   prompt would silently apply the user's "Allow" to a payload they
//!   never saw — TOCTOU at the UI layer (scenarios #2 / #5 / #14 / #49
//!   in the 50-scenario list).
//!
//! - **Stale revalidation** requires content-bound identity: if the
//!   target file changes between "approval shown" and "tool actually
//!   runs", the host must detect that and re-prompt with the new diff.
//!
//! Mixing both jobs onto a single fingerprint is the bug R1 calls
//! out as Critical 1.
//!
//! ## Pairing with `PermissionRuleFingerprint`
//!
//! - `ApprovalRequestKey`           → exact identity, dedup, stale check
//! - `PermissionRuleFingerprint`    → wider pattern, persisted rule, override match
//!
//! See plan v3 §P1.5 for the design rationale and §P4 for how the
//! queue layer uses `ApprovalRequestKey` to merge `Vec<oneshot::Sender>`
//! when (and only when) the keys are byte-equal.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

/// Strict request identity used for approval-queue dedup and
/// pre-execution stale revalidation.
///
/// Two `ApprovalRequestKey`s compare equal **iff they describe the
/// same tool call against the same payload**. A queue may merge
/// pending entries with equal keys (and broadcast the user's choice
/// to every waiting `oneshot::Sender`); it MUST NOT merge entries
/// with unequal keys, even if they hash to the same broader rule
/// pattern.
///
/// Construct via [`ApprovalRequestKey::new`] which canonicalizes the
/// cwd and computes both `args_hash` and `payload_hash` for you.
/// Direct field construction is allowed but reserved for tests / the
/// runtime gate that already has the digests in hand.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApprovalRequestKey {
    /// Tool name verbatim — NOT lowercased. `mcp_github_create_issue`
    /// and `mcp_GitHub_create_issue` are distinct because remote
    /// servers can be case-sensitive; we don't second-guess.
    pub tool: String,
    /// Canonicalized working directory.
    ///
    /// On the host side we run `Path::canonicalize` before storing so
    /// `./foo` and `/abs/path/foo` resolve to the same key. Falls back
    /// to the original buf if canonicalization fails (e.g. the path
    /// doesn't exist yet for a tool that's about to create it); in
    /// that case the caller is responsible for any lexical
    /// normalization they want.
    pub canonical_cwd: PathBuf,
    /// SHA-256 of the canonical-JSON-serialized arguments.
    ///
    /// "Canonical JSON" here means: serde_json::Value with object keys
    /// sorted lexicographically (we use [`canonical_args_json`]). Two
    /// argument blobs that serialize to equivalent JSON but differ in
    /// key order are considered equal.
    pub args_hash: [u8; 32],
    /// Optional payload-content hash (e.g. for `edit_file`: SHA-256 of
    /// `base_content || patch`).
    ///
    /// `None` for tools where no extra payload is meaningful. When
    /// present, the host computes this by reading the target file
    /// **at enqueue time** and storing the result in
    /// `PendingApproval.base_digest` — see plan v3 §P5f. We
    /// deliberately do NOT trust an `expected_base_sha` arg from the
    /// LLM (review-r2 Major 3); the host is the source of truth.
    pub payload_hash: Option<[u8; 32]>,
    /// If a sub-agent issued this request, its identifier. The TUI
    /// renders `[agent: foo]` chips and the approval card disables
    /// persistent-scope buttons (P3 §source-agent) so a child can't
    /// silently extend the project rule file.
    pub source_agent: Option<String>,
    /// Identifier of the LLM round (turn) this request belongs to.
    /// Used for "rest-of-turn" scope decisions (P3) and to invalidate
    /// pending approvals when the user cancels a turn.
    pub turn_id: Uuid,
}

impl ApprovalRequestKey {
    /// Construct a key from raw inputs, canonicalizing the cwd and
    /// hashing the args for you.
    ///
    /// `payload_hash` is left as `None` — file-edit tools should call
    /// [`with_payload_hash`] after the host has computed the base
    /// digest, NOT before. This keeps the "host is the source of truth
    /// for stale revalidation" contract auditable: every place that
    /// produces a payload hash in the codebase is in the host gate.
    #[must_use]
    pub fn new(
        tool: impl Into<String>,
        cwd: impl Into<PathBuf>,
        args: &serde_json::Value,
        source_agent: Option<String>,
        turn_id: Uuid,
    ) -> Self {
        let cwd_path: PathBuf = cwd.into();
        let canonical_cwd = std::fs::canonicalize(&cwd_path).unwrap_or(cwd_path);
        let args_hash = hash_canonical_json(args);
        Self {
            tool: tool.into(),
            canonical_cwd,
            args_hash,
            payload_hash: None,
            source_agent,
            turn_id,
        }
    }

    /// Attach a host-computed payload hash. Call this from the gate
    /// when stat'ing the target file at enqueue time.
    #[must_use]
    pub fn with_payload_hash(mut self, hash: [u8; 32]) -> Self {
        self.payload_hash = Some(hash);
        self
    }
}

/// Compute SHA-256 of `args` after canonical-JSON serialization
/// (object keys sorted recursively).
///
/// We canonicalize to make `{"a": 1, "b": 2}` and `{"b": 2, "a": 1}`
/// hash identically — the LLM may emit keys in any order on retry,
/// and we don't want a re-tried tool call to bypass dedup.
#[must_use]
pub fn hash_canonical_json(value: &serde_json::Value) -> [u8; 32] {
    let canonical = canonical_args_json(value);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hasher.finalize().into()
}

/// Serialize `value` to JSON with object keys sorted recursively.
#[must_use]
pub fn canonical_args_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value);
    out
}

fn write_canonical(out: &mut String, value: &serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // Reuse serde_json's escaping for strings.
            out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| String::from("\"\"")));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let key_json =
                    serde_json::to_string(*key).unwrap_or_else(|_| String::from("\"\""));
                out.push_str(&key_json);
                out.push(':');
                write_canonical(out, &map[*key]);
            }
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixed_turn() -> Uuid {
        Uuid::nil()
    }

    #[test]
    fn equal_args_hash_to_same_value() {
        let a = json!({"path": "src/lib.rs", "content": "// hi"});
        let b = json!({"content": "// hi", "path": "src/lib.rs"}); // different key order
        assert_eq!(hash_canonical_json(&a), hash_canonical_json(&b));
    }

    #[test]
    fn different_args_hash_to_different_value() {
        let a = json!({"path": "src/lib.rs"});
        let b = json!({"path": "src/main.rs"});
        assert_ne!(hash_canonical_json(&a), hash_canonical_json(&b));
    }

    #[test]
    fn nested_object_keys_are_canonicalized() {
        let a = json!({"outer": {"a": 1, "b": 2}});
        let b = json!({"outer": {"b": 2, "a": 1}});
        assert_eq!(hash_canonical_json(&a), hash_canonical_json(&b));
    }

    #[test]
    fn array_order_is_preserved() {
        // Arrays are NOT sorted — order is meaningful for command
        // arguments, file paths, etc.
        let a = json!({"argv": ["a", "b"]});
        let b = json!({"argv": ["b", "a"]});
        assert_ne!(hash_canonical_json(&a), hash_canonical_json(&b));
    }

    #[test]
    fn key_equality_requires_all_fields() {
        let cwd = std::env::temp_dir();
        let args = json!({"command": "ls"});

        let k1 = ApprovalRequestKey::new("bash", &cwd, &args, None, fixed_turn());
        let k2 = ApprovalRequestKey::new("bash", &cwd, &args, None, fixed_turn());
        assert_eq!(k1, k2);

        let k3 = ApprovalRequestKey::new("bash", &cwd, &args, Some("child".into()), fixed_turn());
        assert_ne!(k1, k3, "different source_agent must yield different keys");

        let other_args = json!({"command": "pwd"});
        let k4 = ApprovalRequestKey::new("bash", &cwd, &other_args, None, fixed_turn());
        assert_ne!(k1, k4, "different args must yield different keys");

        let k5 = ApprovalRequestKey::new("write_file", &cwd, &args, None, fixed_turn());
        assert_ne!(k1, k5, "different tool must yield different keys");
    }

    #[test]
    fn payload_hash_distinguishes_otherwise_equal_keys() {
        let cwd = std::env::temp_dir();
        let args = json!({"path": "src/lib.rs", "patch": "abc"});
        let base = ApprovalRequestKey::new("write_file", &cwd, &args, None, fixed_turn());

        let with_v1 = base.clone().with_payload_hash([1u8; 32]);
        let with_v2 = base.clone().with_payload_hash([2u8; 32]);

        assert_ne!(
            with_v1, with_v2,
            "different payload_hash must produce different keys"
        );
        assert_ne!(
            base, with_v1,
            "missing payload_hash != present payload_hash"
        );
    }
}
