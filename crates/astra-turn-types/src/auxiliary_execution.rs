//! Persistable effective generation policy for one auxiliary operation/purpose.
use crate::{InferencePurpose, ThinkingConfig};
use serde::{Deserialize, Serialize};

pub const AUXILIARY_GENERATION_POLICY_VERSION: u32 = 1;

/// Captured Work-classifier policy; boundary eligibility remains turn-local.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkAdmissionGate {
    Allowed,
    Disabled,
    BoundaryOnly,
}

impl WorkAdmissionGate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Disabled => "disabled",
            Self::BoundaryOnly => "boundary_only",
        }
    }
}

/// Captured admission decision for optional auxiliary calls. This required
/// enum has no default or deserialization aliases; Work admission is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryCallGate {
    Allowed,
    Disabled,
    BoundaryOnly,
    ProviderAdmissionEnabled,
}

impl AuxiliaryCallGate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Disabled => "disabled",
            Self::BoundaryOnly => "boundary_only",
            Self::ProviderAdmissionEnabled => "provider_admission_enabled",
        }
    }
}

/// Final temperature decision for one auxiliary inference call.
///
/// This is deliberately distinct from `Option<f64>`: both an inherited route
/// default and an asserted provider default are represented by `None` at the
/// lower-level client boundary, but only the former may contain a configured
/// body override. Resolution below validates that distinction before provider
/// I/O.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AuxiliaryTemperatureEmission {
    InheritRouteDefault,
    ProviderDefault,
    Forbidden,
    Explicit(f64),
}

impl AuxiliaryTemperatureEmission {
    pub fn call_temperature(self) -> Option<f64> {
        match self {
            Self::Explicit(value) => Some(value),
            Self::InheritRouteDefault | Self::ProviderDefault | Self::Forbidden => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InheritRouteDefault => "inherit_route_default",
            Self::ProviderDefault => "provider_default",
            Self::Forbidden => "forbidden",
            Self::Explicit(_) => "explicit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuxiliaryPolicyProvenance {
    ExistingPurposePolicy,
    OfferingCapability,
    CanonicalProviderContract,
    ConservativeProtocolDefault,
}

impl AuxiliaryPolicyProvenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExistingPurposePolicy => "existing_purpose_policy",
            Self::OfferingCapability => "offering_capability",
            Self::CanonicalProviderContract => "canonical_provider_contract",
            Self::ConservativeProtocolDefault => "conservative_protocol_default",
        }
    }
}

/// Resolved without credentials or private route material. The owning execution
/// freeze must bind this policy to the exact admitted model and transport.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliaryGenerationPolicy {
    pub schema_version: u32,
    pub operation_id: String,
    pub purpose: InferencePurpose,
    pub thinking: ThinkingConfig,
    pub temperature: AuxiliaryTemperatureEmission,
    pub temperature_provenance: AuxiliaryPolicyProvenance,
    /// Validated route temperature, including a fixed value or body override.
    /// Captured even when emission inherits the route or thinking suppresses it.
    pub configured_temperature: Option<f64>,
    pub max_output_tokens: usize,
}
