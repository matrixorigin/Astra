//! Protocol-independent provider discovery and tool identity contracts.
//!
//! Provider adapters decode wire-specific declarations into these portable
//! facts. They intentionally do not decide permission, retry, caching, prompt
//! placement, or result projection policy.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

macro_rules! non_empty_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderContractError> {
                let value = value.into();
                if value.trim().is_empty() {
                    return Err(ProviderContractError::EmptyIdentifier { kind: $kind });
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = ProviderContractError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

non_empty_id!(ProviderIdentity, "provider_identity");
non_empty_id!(ProviderBindingRef, "provider_binding_ref");
non_empty_id!(ProviderProtocolId, "provider_protocol_id");
non_empty_id!(NativeToolId, "native_tool_id");
non_empty_id!(DescriptorVersion, "descriptor_version");
non_empty_id!(ProviderRejectionCode, "provider_rejection_code");

/// Stable internal tool identity. Model-visible aliases are deliberately not
/// part of this key.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ToolIdentity {
    pub provider_binding: ProviderBindingRef,
    pub native_tool_id: NativeToolId,
}

impl ToolIdentity {
    pub fn new(provider_binding: ProviderBindingRef, native_tool_id: NativeToolId) -> Self {
        Self {
            provider_binding,
            native_tool_id,
        }
    }
}

/// Exact resolved descriptor used by an invocation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResolvedToolDescriptorRef {
    pub identity: ToolIdentity,
    pub descriptor_version: DescriptorVersion,
}

impl ResolvedToolDescriptorRef {
    pub fn new(
        identity: ToolIdentity,
        descriptor_version: impl Into<String>,
    ) -> Result<Self, ProviderContractError> {
        Ok(Self {
            identity,
            descriptor_version: DescriptorVersion::new(descriptor_version)?,
        })
    }
}

/// Provenance for one provider declaration claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderClaimSource {
    StandardProtocol {
        protocol: ProviderProtocolId,
        field: String,
    },
    ProviderExtension {
        namespace: String,
        field: String,
    },
    AstraOwned {
        component: String,
        field: String,
    },
}

/// A claim and its origin. Trust is assigned by Astra's resolver, not by the
/// adapter that decoded the claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderClaim<T> {
    pub value: T,
    pub source: ProviderClaimSource,
}

impl<T> ProviderClaim<T> {
    pub fn new(value: T, source: ProviderClaimSource) -> Self {
        Self { value, source }
    }
}

/// Orthogonal provider hints. Absence remains distinct from `false`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent: Option<ProviderClaim<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<ProviderClaim<bool>>,
}

/// Provider-declared support for asynchronous/task-augmented execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTaskSupport {
    #[default]
    Unspecified,
    Forbidden,
    Optional,
    Required,
}

/// Losslessly normalized tool declaration before Astra policy resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderToolDeclaration {
    pub native_tool_id: NativeToolId,
    pub native_tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub claims: ProviderToolClaims,
    #[serde(default)]
    pub task_support: ProviderTaskSupport,
    /// Protocol/provider fields that do not yet have a portable Astra
    /// semantic. Keys must be namespace-qualified by the adapter.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extension_fields: Map<String, Value>,
}

impl ProviderToolDeclaration {
    pub fn validate(&self) -> Result<(), ProviderContractError> {
        if self.native_tool_name.trim().is_empty() {
            return Err(ProviderContractError::EmptyIdentifier {
                kind: "native_tool_name",
            });
        }
        if !self.input_schema.is_object() {
            return Err(ProviderContractError::SchemaMustBeObject {
                native_tool_id: self.native_tool_id.to_string(),
                field: "input_schema",
            });
        }
        if self
            .output_schema
            .as_ref()
            .is_some_and(|schema| !schema.is_object())
        {
            return Err(ProviderContractError::SchemaMustBeObject {
                native_tool_id: self.native_tool_id.to_string(),
                field: "output_schema",
            });
        }
        for source in [
            self.claims.read_only.as_ref().map(|claim| &claim.source),
            self.claims.destructive.as_ref().map(|claim| &claim.source),
            self.claims.idempotent.as_ref().map(|claim| &claim.source),
            self.claims.open_world.as_ref().map(|claim| &claim.source),
        ]
        .into_iter()
        .flatten()
        {
            validate_claim_source(source)?;
        }
        for key in self.extension_fields.keys() {
            let qualified = key
                .split_once('.')
                .is_some_and(|(namespace, field)| !namespace.is_empty() && !field.is_empty());
            if !qualified {
                return Err(ProviderContractError::UnqualifiedExtensionField {
                    native_tool_id: self.native_tool_id.to_string(),
                    field: key.clone(),
                });
            }
        }
        Ok(())
    }

    fn canonicalize_json(&mut self) {
        self.input_schema = canonical_json(&self.input_schema);
        self.output_schema = self.output_schema.as_ref().map(canonical_json);
        let extension_fields = Value::Object(std::mem::take(&mut self.extension_fields));
        let Value::Object(extension_fields) = canonical_json(&extension_fields) else {
            unreachable!("canonicalizing a JSON object must preserve its value kind");
        };
        self.extension_fields = extension_fields;
    }
}

/// Immutable, content-addressed discovery snapshot for one provider binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderDiscoverySnapshot {
    pub provider_identity: ProviderIdentity,
    pub binding_ref: ProviderBindingRef,
    pub protocol: ProviderProtocolId,
    pub tool_declarations: Vec<ProviderToolDeclaration>,
    pub content_hash: String,
}

#[derive(Deserialize)]
struct ProviderDiscoverySnapshotWire {
    provider_identity: ProviderIdentity,
    binding_ref: ProviderBindingRef,
    protocol: ProviderProtocolId,
    tool_declarations: Vec<ProviderToolDeclaration>,
    content_hash: String,
}

impl<'de> Deserialize<'de> for ProviderDiscoverySnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ProviderDiscoverySnapshotWire::deserialize(deserializer)?;
        let supplied_hash = wire.content_hash;
        let snapshot = Self::new(
            wire.provider_identity,
            wire.binding_ref,
            wire.protocol,
            wire.tool_declarations,
        )
        .map_err(serde::de::Error::custom)?;
        if supplied_hash != snapshot.content_hash {
            return Err(serde::de::Error::custom(
                ProviderContractError::ContentHashMismatch {
                    supplied: supplied_hash,
                    computed: snapshot.content_hash,
                },
            ));
        }
        Ok(snapshot)
    }
}

impl ProviderDiscoverySnapshot {
    pub fn new(
        provider_identity: ProviderIdentity,
        binding_ref: ProviderBindingRef,
        protocol: ProviderProtocolId,
        mut tool_declarations: Vec<ProviderToolDeclaration>,
    ) -> Result<Self, ProviderContractError> {
        for declaration in &mut tool_declarations {
            declaration.validate()?;
            declaration.canonicalize_json();
        }
        tool_declarations.sort_by(|left, right| {
            left.native_tool_id
                .cmp(&right.native_tool_id)
                .then_with(|| left.native_tool_name.cmp(&right.native_tool_name))
        });

        let mut seen = BTreeSet::new();
        for declaration in &tool_declarations {
            if !seen.insert(declaration.native_tool_id.clone()) {
                return Err(ProviderContractError::DuplicateNativeToolId {
                    native_tool_id: declaration.native_tool_id.to_string(),
                });
            }
        }

        let hash_input = (
            &provider_identity,
            &binding_ref,
            &protocol,
            &tool_declarations,
        );
        let encoded = serde_json::to_vec(&hash_input)
            .map_err(|error| ProviderContractError::Serialization(error.to_string()))?;
        let content_hash = format!("{:x}", Sha256::digest(encoded));

        Ok(Self {
            provider_identity,
            binding_ref,
            protocol,
            tool_declarations,
            content_hash,
        })
    }

    pub fn tool_identity(&self, declaration: &ProviderToolDeclaration) -> ToolIdentity {
        ToolIdentity::new(self.binding_ref.clone(), declaration.native_tool_id.clone())
    }
}

/// Provider result payload before model/client projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderCallPayload {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_metadata: Option<Value>,
}

/// A provider acknowledged the request but declined to execute it. This is
/// distinct from an Astra admission rejection and from a transport failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRejection {
    pub code: ProviderRejectionCode,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

impl ProviderRejection {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, ProviderContractError> {
        Ok(Self {
            code: ProviderRejectionCode::new(code)?,
            message: message.into(),
            retryable,
        })
    }
}

/// Acknowledged provider tool outcome. Transport/protocol failures remain in
/// the adapter's error channel and carry dispatch certainty there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "payload", rename_all = "snake_case")]
pub enum ProviderCallOutcome {
    Success(ProviderCallPayload),
    ToolFailure(ProviderCallPayload),
    Rejected(ProviderRejection),
}

impl ProviderCallOutcome {
    pub fn payload(&self) -> Option<&ProviderCallPayload> {
        match self {
            Self::Success(payload) | Self::ToolFailure(payload) => Some(payload),
            Self::Rejected(_) => None,
        }
    }

    pub fn is_error(&self) -> bool {
        !matches!(self, Self::Success(_))
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProviderContractError {
    #[error("{kind} must not be empty")]
    EmptyIdentifier { kind: &'static str },
    #[error("duplicate native tool id '{native_tool_id}' in provider snapshot")]
    DuplicateNativeToolId { native_tool_id: String },
    #[error("tool '{native_tool_id}' {field} must be a JSON object")]
    SchemaMustBeObject {
        native_tool_id: String,
        field: &'static str,
    },
    #[error("tool '{native_tool_id}' extension field '{field}' must be namespace-qualified")]
    UnqualifiedExtensionField {
        native_tool_id: String,
        field: String,
    },
    #[error(
        "provider snapshot content hash mismatch: supplied '{supplied}', computed '{computed}'"
    )]
    ContentHashMismatch { supplied: String, computed: String },
    #[error("failed to serialize provider snapshot: {0}")]
    Serialization(String),
}

fn validate_claim_source(source: &ProviderClaimSource) -> Result<(), ProviderContractError> {
    let (kind, first, field) = match source {
        ProviderClaimSource::StandardProtocol { field, .. } => {
            ("provider_claim_protocol", None, field)
        }
        ProviderClaimSource::ProviderExtension { namespace, field } => {
            ("provider_claim_extension", Some(namespace), field)
        }
        ProviderClaimSource::AstraOwned { component, field } => {
            ("provider_claim_astra_component", Some(component), field)
        }
    };
    if first.is_some_and(|value| value.trim().is_empty()) || field.trim().is_empty() {
        return Err(ProviderContractError::EmptyIdentifier { kind });
    }
    Ok(())
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(object) => {
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut canonical = Map::new();
            for key in keys {
                canonical.insert(key.clone(), canonical_json(&object[key]));
            }
            Value::Object(canonical)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn declaration(id: &str, schema: Value) -> ProviderToolDeclaration {
        ProviderToolDeclaration {
            native_tool_id: NativeToolId::new(id).unwrap(),
            native_tool_name: id.to_string(),
            title: None,
            description: None,
            input_schema: schema,
            output_schema: None,
            claims: ProviderToolClaims::default(),
            task_support: ProviderTaskSupport::Unspecified,
            extension_fields: Map::new(),
        }
    }

    fn snapshot(tools: Vec<ProviderToolDeclaration>) -> ProviderDiscoverySnapshot {
        ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            tools,
        )
        .unwrap()
    }

    #[test]
    fn identifiers_reject_whitespace_only_values_including_deserialization() {
        assert!(ProviderIdentity::new("  ").is_err());
        let parsed = serde_json::from_str::<ProviderBindingRef>(r#"""#);
        assert!(parsed.is_err());
    }

    #[test]
    fn snapshot_hash_is_independent_of_discovery_and_object_key_order() {
        let first = snapshot(vec![
            declaration(
                "z",
                json!({"type": "object", "properties": {"b": {}, "a": {}}}),
            ),
            declaration("a", json!({"required": ["q"], "type": "object"})),
        ]);

        let mut reversed_properties = Map::new();
        reversed_properties.insert("a".to_string(), json!({}));
        reversed_properties.insert("b".to_string(), json!({}));
        let second = snapshot(vec![
            declaration("a", json!({"type": "object", "required": ["q"]})),
            declaration(
                "z",
                json!({"properties": reversed_properties, "type": "object"}),
            ),
        ]);

        assert_eq!(first.content_hash, second.content_hash);
        assert_eq!(first.tool_declarations, second.tool_declarations);
    }

    #[test]
    fn snapshot_rejects_duplicate_native_identity() {
        let error = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            vec![
                declaration("same", json!({"type": "object"})),
                declaration("same", json!({"type": "object"})),
            ],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            ProviderContractError::DuplicateNativeToolId { .. }
        ));
    }

    #[test]
    fn snapshot_rejects_unqualified_extension_fields() {
        let mut tool = declaration("read", json!({"type": "object"}));
        tool.extension_fields
            .insert("metadata".to_string(), json!({"safe": true}));

        let error = ProviderDiscoverySnapshot::new(
            ProviderIdentity::new("provider-a").unwrap(),
            ProviderBindingRef::new("binding-a").unwrap(),
            ProviderProtocolId::new("test").unwrap(),
            vec![tool],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ProviderContractError::UnqualifiedExtensionField { .. }
        ));
    }

    #[test]
    fn deserialization_recomputes_and_rejects_a_tampered_snapshot_hash() {
        let snapshot = snapshot(vec![declaration("read", json!({"type": "object"}))]);
        let mut serialized = serde_json::to_value(&snapshot).unwrap();
        let restored =
            serde_json::from_value::<ProviderDiscoverySnapshot>(serialized.clone()).unwrap();
        assert_eq!(restored, snapshot);

        serialized["content_hash"] = Value::String("forged".to_string());

        let error = serde_json::from_value::<ProviderDiscoverySnapshot>(serialized).unwrap_err();
        assert!(error.to_string().contains("content hash mismatch"));
    }

    #[test]
    fn real_semantic_changes_invalidate_snapshot_hash() {
        let original = snapshot(vec![declaration(
            "read",
            json!({"type": "object", "properties": {}}),
        )]);
        let changed = snapshot(vec![declaration(
            "read",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);

        assert_ne!(original.content_hash, changed.content_hash);
    }

    #[test]
    fn public_alias_is_not_part_of_internal_identity() {
        let snapshot = snapshot(vec![declaration("native.tool", json!({"type": "object"}))]);
        let identity = snapshot.tool_identity(&snapshot.tool_declarations[0]);

        assert_eq!(identity.native_tool_id.as_str(), "native.tool");
        assert_eq!(identity.provider_binding.as_str(), "binding-a");
    }

    #[test]
    fn typed_provider_outcome_never_infers_failure_from_text() {
        let success = ProviderCallOutcome::Success(ProviderCallPayload {
            text: "error: this is quoted documentation".to_string(),
            structured_content: None,
            protocol_metadata: None,
        });
        let failure = ProviderCallOutcome::ToolFailure(ProviderCallPayload {
            text: "ok".to_string(),
            structured_content: None,
            protocol_metadata: None,
        });

        assert!(!success.is_error());
        assert!(failure.is_error());
    }

    #[test]
    fn provider_rejection_requires_a_machine_readable_code() {
        assert!(ProviderRejection::new(" ", "busy", true).is_err());
        let rejection = ProviderCallOutcome::Rejected(
            ProviderRejection::new("capacity_exhausted", "busy", true).unwrap(),
        );
        assert!(rejection.is_error());
        assert_eq!(rejection.payload(), None);
    }
}
