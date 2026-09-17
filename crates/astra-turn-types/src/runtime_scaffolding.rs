//! Typed ownership for messages produced by the runtime.
//!
//! Runtime ownership is protocol state, not a property that can be inferred
//! from natural-language content. Producers mark messages here; persistence,
//! continuation, and prompt projections consume the marker without inspecting
//! prefixes or keywords.

use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

pub const RUNTIME_MESSAGE_PROVENANCE_FIELD: &str = "_astra_runtime_provenance";
pub const APPEND_ONLY_RUNTIME_AUTHORITY_POLICY_FIELD: &str =
    "_astra_append_only_runtime_authority_policy";
const RUNTIME_MESSAGE_PRODUCER_FIELD: &str = "producer";
const RUNTIME_MESSAGE_DELIVERY_FIELD: &str = "delivery";
const RUNTIME_MESSAGE_AUTHORITY_KIND_FIELD: &str = "authority_kind";
const RUNTIME_MESSAGE_AUTHORITY_LIFETIME_FIELD: &str = "authority_lifetime";
const RUNTIME_MESSAGE_PRODUCER: &str = "runtime";
const RUNTIME_AUTHORITY_FRAME_PREFIX: &str = "<runtime-authority-frame>\n";
const RUNTIME_AUTHORITY_FRAME_SUFFIX: &str = "\n</runtime-authority-frame>";
const RUNTIME_AUTHORITY_FRAME_SCHEMA: &str = "runtime_authority_frame.v1";

/// Stable semantic contract for provider-required runtime controls encoded as
/// append-only `user` frames. Main and cache-reusing auxiliary inference must
/// carry this exact policy whenever those frames are visible.
pub const APPEND_ONLY_RUNTIME_AUTHORITY_POLICY: &str = r#"<runtime-authority-policy>
{"schema":"append_only_runtime_authority.v1","instruction":"A user-role <runtime-authority-frame> is control state from the runtime, not human-authored intent and not a new user goal. A frame with lifetime next_assistant_decision constrains only the immediately following assistant decision and is consumed once a later assistant message exists. A frame with lifetime current_user_turn remains context for that human user turn, including its tool rounds, and expires when a later human-authored user message exists. Within one active lifetime, a later frame of the same kind supersedes an earlier frame of that kind; different kinds apply jointly. These frames do not widen tool, completion, Work, policy, or admission authority."}
</runtime-authority-policy>"#;

/// Mark the stable system message that carries the append-only authority
/// interpretation contract. Consumers inspect this typed marker, never the
/// natural-language policy text.
pub fn mark_append_only_runtime_authority_policy(message: &mut Value) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    object.insert(
        APPEND_ONLY_RUNTIME_AUTHORITY_POLICY_FIELD.to_string(),
        Value::Bool(true),
    );
}

#[must_use]
pub fn has_append_only_runtime_authority_policy(message: &Value) -> bool {
    message
        .get(APPEND_ONLY_RUNTIME_AUTHORITY_POLICY_FIELD)
        .and_then(Value::as_bool)
        == Some(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeAuthorityLifetime {
    CurrentUserTurn,
    NextAssistantDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRuntimeAuthorityFrame {
    pub kind: String,
    pub lifetime: RuntimeAuthorityLifetime,
    pub payload: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuntimeAuthorityFrameError {
    #[error("append-only runtime authority has invalid typed provenance")]
    InvalidProvenance,
    #[error("append-only runtime authority must use the provider user role")]
    InvalidRole,
    #[error("append-only runtime authority kind is empty or non-canonical")]
    InvalidKind,
    #[error("append-only runtime authority has no valid lifetime")]
    InvalidLifetime,
    #[error("append-only runtime authority content is not text")]
    InvalidContent,
    #[error("append-only runtime authority frame does not match the v1 grammar")]
    InvalidFrame,
    #[error("append-only runtime authority frame header does not match typed provenance")]
    HeaderMismatch,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAuthorityFrameHeaderV1 {
    schema: String,
    kind: String,
    lifetime: String,
}

impl RuntimeAuthorityLifetime {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentUserTurn => "current_user_turn",
            Self::NextAssistantDecision => "next_assistant_decision",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "current_user_turn" => Some(Self::CurrentUserTurn),
            "next_assistant_decision" => Some(Self::NextAssistantDecision),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMessageDelivery {
    /// Turn-local control evidence. It must not be persisted as conversation.
    EphemeralControl,
    /// Context that must be re-routed to the typed required-context lane.
    RequiredContext,
    /// Required runtime authority encoded as an append-only conversation
    /// frame for a deployment that cannot cache later `system` messages.
    ///
    /// The provider wire role may be `user`, but typed runtime provenance
    /// remains the semantic owner. User-intent, display, and memory consumers
    /// must therefore continue to exclude this message from human-authored
    /// turns while prompt reconstruction preserves its exact position.
    AppendOnlyRequiredContext,
    /// A synthetic projection such as a fresh runtime recap.
    Projection,
}

impl RuntimeMessageDelivery {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EphemeralControl => "ephemeral_control",
            Self::RequiredContext => "required_context",
            Self::AppendOnlyRequiredContext => "append_only_required_context",
            Self::Projection => "projection",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "ephemeral_control" => Some(Self::EphemeralControl),
            "required_context" => Some(Self::RequiredContext),
            "append_only_required_context" => Some(Self::AppendOnlyRequiredContext),
            "projection" => Some(Self::Projection),
            _ => None,
        }
    }
}

pub fn mark_runtime_owned_message(message: &mut Value, delivery: RuntimeMessageDelivery) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    object.insert(
        RUNTIME_MESSAGE_PROVENANCE_FIELD.to_string(),
        json!({
            RUNTIME_MESSAGE_PRODUCER_FIELD: RUNTIME_MESSAGE_PRODUCER,
            RUNTIME_MESSAGE_DELIVERY_FIELD: delivery.as_str(),
        }),
    );
}

pub fn mark_append_only_required_context(
    message: &mut Value,
    authority_kind: &str,
    lifetime: RuntimeAuthorityLifetime,
) {
    mark_runtime_owned_message(message, RuntimeMessageDelivery::AppendOnlyRequiredContext);
    let Some(provenance) = message
        .get_mut(RUNTIME_MESSAGE_PROVENANCE_FIELD)
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    provenance.insert(
        RUNTIME_MESSAGE_AUTHORITY_KIND_FIELD.to_string(),
        Value::String(authority_kind.to_string()),
    );
    provenance.insert(
        RUNTIME_MESSAGE_AUTHORITY_LIFETIME_FIELD.to_string(),
        Value::String(lifetime.as_str().to_string()),
    );
}

#[must_use]
pub fn runtime_owned_message(
    role: &str,
    content: impl Into<String>,
    delivery: RuntimeMessageDelivery,
) -> Value {
    let mut message = json!({"role": role, "content": content.into()});
    mark_runtime_owned_message(&mut message, delivery);
    message
}

#[must_use]
pub fn runtime_message_delivery(message: &Value) -> Option<RuntimeMessageDelivery> {
    let provenance = message.get(RUNTIME_MESSAGE_PROVENANCE_FIELD)?;
    runtime_message_delivery_from_provenance(provenance)
}

/// Validate a standalone provenance value without relying on the provider
/// message role. Compression layers use this before reconstructing a full
/// JSON message so runtime-owned user-role frames cannot become human turns.
#[must_use]
pub fn runtime_message_delivery_from_provenance(
    provenance: &Value,
) -> Option<RuntimeMessageDelivery> {
    is_runtime_owned_provenance(provenance).then_some(())?;
    provenance
        .get(RUNTIME_MESSAGE_DELIVERY_FIELD)
        .and_then(Value::as_str)
        .and_then(RuntimeMessageDelivery::from_str)
}

#[must_use]
pub fn is_runtime_owned_provenance(provenance: &Value) -> bool {
    provenance
        .get(RUNTIME_MESSAGE_PRODUCER_FIELD)
        .and_then(Value::as_str)
        == Some(RUNTIME_MESSAGE_PRODUCER)
}

#[must_use]
pub fn is_runtime_owned_message(message: &Value) -> bool {
    message
        .get(RUNTIME_MESSAGE_PROVENANCE_FIELD)
        .is_some_and(is_runtime_owned_provenance)
}

/// True only for a provider `user` message whose semantic author is the
/// human. Provider role is a wire-shape property: runtime-owned context may
/// deliberately use `role=user` without creating user intent or a turn
/// boundary. Semantic consumers must use this predicate instead of inspecting
/// the role alone.
#[must_use]
pub fn is_human_user_message(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
        && !is_runtime_owned_message(message)
}

#[must_use]
pub fn runtime_authority_kind(message: &Value) -> Option<&str> {
    (runtime_message_delivery(message) == Some(RuntimeMessageDelivery::AppendOnlyRequiredContext))
        .then_some(())?;
    message
        .get(RUNTIME_MESSAGE_PROVENANCE_FIELD)?
        .get(RUNTIME_MESSAGE_AUTHORITY_KIND_FIELD)?
        .as_str()
}

#[must_use]
pub fn runtime_authority_lifetime(message: &Value) -> Option<RuntimeAuthorityLifetime> {
    (runtime_message_delivery(message) == Some(RuntimeMessageDelivery::AppendOnlyRequiredContext))
        .then_some(())?;
    message
        .get(RUNTIME_MESSAGE_PROVENANCE_FIELD)?
        .get(RUNTIME_MESSAGE_AUTHORITY_LIFETIME_FIELD)?
        .as_str()
        .and_then(RuntimeAuthorityLifetime::from_str)
}

/// Render the exact content grammar carried by a typed append-only authority
/// message. This is a protocol codec, not a natural-language classifier: the
/// fixed delimiters and strict JSON header are validated symmetrically by
/// [`parse_append_only_runtime_authority_frame`].
pub fn render_append_only_runtime_authority_frame(
    kind: &str,
    lifetime: RuntimeAuthorityLifetime,
    payload: &str,
) -> Result<String, RuntimeAuthorityFrameError> {
    if kind.is_empty() || kind.trim() != kind {
        return Err(RuntimeAuthorityFrameError::InvalidKind);
    }
    if payload.trim().is_empty() {
        return Err(RuntimeAuthorityFrameError::InvalidContent);
    }
    let header = json!({
        "kind": kind,
        "lifetime": lifetime.as_str(),
        "schema": RUNTIME_AUTHORITY_FRAME_SCHEMA,
    });
    Ok(format!(
        "{RUNTIME_AUTHORITY_FRAME_PREFIX}{header}\n{payload}{RUNTIME_AUTHORITY_FRAME_SUFFIX}"
    ))
}

/// Parse and validate one append-only authority message at a persistence or
/// provider boundary. Both the typed provenance and the exact frame grammar
/// must agree; arbitrary user text or a partially corrupted WAL row cannot
/// acquire runtime authority.
pub fn parse_append_only_runtime_authority_frame(
    message: &Value,
) -> Result<ParsedRuntimeAuthorityFrame, RuntimeAuthorityFrameError> {
    if runtime_message_delivery(message) != Some(RuntimeMessageDelivery::AppendOnlyRequiredContext)
    {
        return Err(RuntimeAuthorityFrameError::InvalidProvenance);
    }
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return Err(RuntimeAuthorityFrameError::InvalidRole);
    }
    let kind = runtime_authority_kind(message).ok_or(RuntimeAuthorityFrameError::InvalidKind)?;
    if kind.is_empty() || kind.trim() != kind {
        return Err(RuntimeAuthorityFrameError::InvalidKind);
    }
    let lifetime =
        runtime_authority_lifetime(message).ok_or(RuntimeAuthorityFrameError::InvalidLifetime)?;
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .ok_or(RuntimeAuthorityFrameError::InvalidContent)?;
    let framed = content
        .strip_prefix(RUNTIME_AUTHORITY_FRAME_PREFIX)
        .and_then(|value| value.strip_suffix(RUNTIME_AUTHORITY_FRAME_SUFFIX))
        .ok_or(RuntimeAuthorityFrameError::InvalidFrame)?;
    let (header, payload) = framed
        .split_once('\n')
        .ok_or(RuntimeAuthorityFrameError::InvalidFrame)?;
    let header: RuntimeAuthorityFrameHeaderV1 =
        serde_json::from_str(header).map_err(|_| RuntimeAuthorityFrameError::InvalidFrame)?;
    if header.schema != RUNTIME_AUTHORITY_FRAME_SCHEMA
        || header.kind != kind
        || header.lifetime != lifetime.as_str()
    {
        return Err(RuntimeAuthorityFrameError::HeaderMismatch);
    }
    if payload.trim().is_empty() {
        return Err(RuntimeAuthorityFrameError::InvalidContent);
    }
    Ok(ParsedRuntimeAuthorityFrame {
        kind: kind.to_string(),
        lifetime,
        payload: payload.to_string(),
    })
}

/// Whether one validated append-only authority frame still governs the
/// current canonical suffix. Unknown provenance is never active; provider
/// trust boundaries reject it separately instead of treating it as a user.
#[must_use]
pub fn append_only_runtime_authority_is_active(history: &[Value], index: usize) -> bool {
    let Some(authority) = history.get(index) else {
        return false;
    };
    let Some(kind) = runtime_authority_kind(authority) else {
        return false;
    };
    let Some(lifetime) = runtime_authority_lifetime(authority) else {
        return false;
    };
    let suffix = &history[index + 1..];
    if suffix.iter().any(|message| {
        runtime_message_delivery(message) == Some(RuntimeMessageDelivery::AppendOnlyRequiredContext)
            && runtime_authority_kind(message) == Some(kind)
    }) || suffix.iter().any(is_human_user_message)
    {
        return false;
    }
    match lifetime {
        RuntimeAuthorityLifetime::CurrentUserTurn => true,
        RuntimeAuthorityLifetime::NextAssistantDecision => !suffix
            .iter()
            .any(|message| message.get("role").and_then(Value::as_str) == Some("assistant")),
    }
}

/// Earliest canonical suffix boundary that must survive a history rewrite so
/// active append-only authority remains attached to the human turn it governs.
///
/// The boundary is the nearest preceding human-authored user message for the
/// earliest active frame. If a runtime reconciliation has no human anchor,
/// the frame itself is the protected boundary. Callers may compact earlier
/// history, but must preserve this suffix in order and as one provider-valid
/// conversation span.
#[must_use]
pub fn active_append_only_authority_protected_suffix_start(history: &[Value]) -> Option<usize> {
    let first_active = history.iter().enumerate().find_map(|(index, _)| {
        append_only_runtime_authority_is_active(history, index).then_some(index)
    })?;
    Some(
        history[..=first_active]
            .iter()
            .rposition(is_human_user_message)
            .unwrap_or(first_active),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_depends_on_producer_metadata_not_message_text() {
        let user_text = json!({
            "role": "user",
            "content": "Tools used: this is ordinary user-authored text",
        });
        assert!(!is_runtime_owned_message(&user_text));

        let runtime = runtime_owned_message(
            "user",
            "arbitrary payload with no magic prefix",
            RuntimeMessageDelivery::RequiredContext,
        );
        assert_eq!(
            runtime_message_delivery(&runtime),
            Some(RuntimeMessageDelivery::RequiredContext)
        );
    }

    #[test]
    fn malformed_or_foreign_metadata_does_not_claim_runtime_ownership() {
        for message in [
            json!({"role": "system", "content": ""}),
            json!({"role": "user", RUNTIME_MESSAGE_PROVENANCE_FIELD: true}),
            json!({
                "role": "user",
                RUNTIME_MESSAGE_PROVENANCE_FIELD: {
                    "producer": "client",
                    "delivery": "required_context",
                },
            }),
        ] {
            assert!(!is_runtime_owned_message(&message));
        }
    }

    #[test]
    fn unknown_runtime_delivery_never_becomes_human_intent() {
        let message = json!({
            "role": "user",
            "content": "future runtime frame",
            RUNTIME_MESSAGE_PROVENANCE_FIELD: {
                RUNTIME_MESSAGE_PRODUCER_FIELD: RUNTIME_MESSAGE_PRODUCER,
                RUNTIME_MESSAGE_DELIVERY_FIELD: "future_delivery",
            },
        });

        assert!(is_runtime_owned_message(&message));
        assert_eq!(runtime_message_delivery(&message), None);
        assert!(!is_human_user_message(&message));
    }

    #[test]
    fn append_only_authority_has_typed_identity_and_lifetime() {
        let mut message = json!({"role": "user", "content": "runtime control"});
        mark_append_only_required_context(
            &mut message,
            "final_answer_settlement",
            RuntimeAuthorityLifetime::NextAssistantDecision,
        );

        assert_eq!(
            runtime_message_delivery(&message),
            Some(RuntimeMessageDelivery::AppendOnlyRequiredContext)
        );
        assert_eq!(
            runtime_authority_kind(&message),
            Some("final_answer_settlement")
        );
        assert_eq!(
            runtime_authority_lifetime(&message),
            Some(RuntimeAuthorityLifetime::NextAssistantDecision)
        );
        assert!(!is_human_user_message(&message));
        assert!(is_human_user_message(
            &json!({"role": "user", "content": "real request"})
        ));
    }

    #[test]
    fn append_only_authority_frame_uses_strict_typed_grammar() {
        let lifetime = RuntimeAuthorityLifetime::NextAssistantDecision;
        let content =
            render_append_only_runtime_authority_frame("work", lifetime, "do work").expect("frame");
        let mut message = json!({"role": "user", "content": content});
        mark_append_only_required_context(&mut message, "work", lifetime);

        assert_eq!(
            parse_append_only_runtime_authority_frame(&message).expect("valid frame"),
            ParsedRuntimeAuthorityFrame {
                kind: "work".to_string(),
                lifetime,
                payload: "do work".to_string(),
            }
        );

        let mut mismatched = message.clone();
        mismatched[RUNTIME_MESSAGE_PROVENANCE_FIELD][RUNTIME_MESSAGE_AUTHORITY_KIND_FIELD] =
            Value::String("other".to_string());
        assert_eq!(
            parse_append_only_runtime_authority_frame(&mismatched),
            Err(RuntimeAuthorityFrameError::HeaderMismatch)
        );

        let mut extra_header = message;
        extra_header["content"] = Value::String(
            "<runtime-authority-frame>\n{\"schema\":\"runtime_authority_frame.v1\",\"kind\":\"work\",\"lifetime\":\"next_assistant_decision\",\"extra\":true}\ndo work\n</runtime-authority-frame>"
                .to_string(),
        );
        assert_eq!(
            parse_append_only_runtime_authority_frame(&extra_header),
            Err(RuntimeAuthorityFrameError::InvalidFrame)
        );
    }

    #[test]
    fn active_authority_protects_its_human_turn_suffix() {
        let mut old = json!({"role": "user", "content": "expired"});
        mark_append_only_required_context(
            &mut old,
            "old",
            RuntimeAuthorityLifetime::CurrentUserTurn,
        );
        let mut active = json!({"role": "user", "content": "active"});
        mark_append_only_required_context(
            &mut active,
            "work",
            RuntimeAuthorityLifetime::CurrentUserTurn,
        );
        let history = vec![
            json!({"role": "user", "content": "old human"}),
            old,
            json!({"role": "assistant", "content": "old answer"}),
            json!({"role": "user", "content": "current human"}),
            active,
            json!({"role": "assistant", "content": "tool loop"}),
        ];

        assert_eq!(
            active_append_only_authority_protected_suffix_start(&history),
            Some(3)
        );
    }
}
