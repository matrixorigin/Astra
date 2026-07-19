//! Typed ownership for messages produced by the runtime.
//!
//! Runtime ownership is protocol state, not a property that can be inferred
//! from natural-language content. Producers mark messages here; persistence,
//! continuation, and prompt projections consume the marker without inspecting
//! prefixes or keywords.

use serde_json::{Value, json};

pub const RUNTIME_MESSAGE_PROVENANCE_FIELD: &str = "_astra_runtime_provenance";
const RUNTIME_MESSAGE_PRODUCER_FIELD: &str = "producer";
const RUNTIME_MESSAGE_DELIVERY_FIELD: &str = "delivery";
const RUNTIME_MESSAGE_PRODUCER: &str = "runtime";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeMessageDelivery {
    /// Turn-local control evidence. It must not be persisted as conversation.
    EphemeralControl,
    /// Context that must be re-routed to the typed required-context lane.
    RequiredContext,
    /// A synthetic projection such as a fresh runtime recap.
    Projection,
}

impl RuntimeMessageDelivery {
    const fn as_str(self) -> &'static str {
        match self {
            Self::EphemeralControl => "ephemeral_control",
            Self::RequiredContext => "required_context",
            Self::Projection => "projection",
        }
    }

    fn from_str(value: &str) -> Option<Self> {
        match value {
            "ephemeral_control" => Some(Self::EphemeralControl),
            "required_context" => Some(Self::RequiredContext),
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
    (provenance
        .get(RUNTIME_MESSAGE_PRODUCER_FIELD)
        .and_then(Value::as_str)
        == Some(RUNTIME_MESSAGE_PRODUCER))
    .then_some(())?;
    provenance
        .get(RUNTIME_MESSAGE_DELIVERY_FIELD)
        .and_then(Value::as_str)
        .and_then(RuntimeMessageDelivery::from_str)
}

#[must_use]
pub fn is_runtime_owned_message(message: &Value) -> bool {
    runtime_message_delivery(message).is_some()
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
}
