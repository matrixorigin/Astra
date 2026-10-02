//! Bounded, redacted request-classification facts in the canonical C3 trace.
//! Classification is not execution authority. Usage and physical requests retain
//! their inference-ledger owner; these facts never assert model adoption.
use crate::JudgmentResponseProvenance;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Number;
pub const SEMANTIC_JUDGMENT_SCHEMA_VERSION: u16 = 1;
pub const SEMANTIC_JUDGMENT_TRACE_ATTR: &str = "semantic_judgment.v1";
pub const SEMANTIC_JUDGMENT_MAX_BYTES: usize = 8_192;
pub const SEMANTIC_JUDGMENT_ID_MAX_BYTES: usize = 512;
pub const SEMANTIC_JUDGMENT_PRESENTATION_MAX_BYTES: usize = 160;
pub const REQUEST_JUDGMENT_MAX_FIELDS: usize = 20;
/// Payload-free validation error; never retains serde/provider error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SemanticJudgmentValidationError {
    #[error("semantic judgment exceeds the observation byte limit")]
    Oversized,
    #[error("invalid semantic judgment contract")]
    InvalidContract,
    #[error("invalid semantic judgment score")]
    InvalidScore,
}

/// An unrounded normalized answer. Discrete provenance encodes categorical
/// false/uncertain/true as 0/0.5/1, not calibrated model confidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SemanticJudgmentScoreV1(Number);

impl SemanticJudgmentScoreV1 {
    pub fn new(value: Number) -> Result<Self, SemanticJudgmentValidationError> {
        if value
            .as_f64()
            .is_some_and(|n| n.is_finite() && (0.0..=1.0).contains(&n))
        {
            Ok(Self(value))
        } else {
            Err(SemanticJudgmentValidationError::InvalidScore)
        }
    }

    pub fn from_f64(value: f64) -> Result<Self, SemanticJudgmentValidationError> {
        Self::new(Number::from_f64(value).ok_or(SemanticJudgmentValidationError::InvalidScore)?)
    }

    pub fn as_number(&self) -> &Number {
        &self.0
    }

    fn is_discrete(&self) -> bool {
        matches!(self.0.as_f64(), Some(0.0 | 0.5 | 1.0))
    }
}

impl<'de> Deserialize<'de> for SemanticJudgmentScoreV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let number = Number::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom("invalid semantic judgment score"))?;
        Self::new(number).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestJudgmentStageV1 {
    Initial,
    Clarification,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RequestJudgmentFieldV1 {
    #[serde(rename = "required")]
    Required,
    #[serde(rename = "defer")]
    Defer,
    #[serde(rename = "mutation.read_only")]
    MutationReadOnly,
    #[serde(rename = "mutation.may_mutate")]
    MutationMayMutate,
    #[serde(rename = "mutation.must_mutate")]
    MutationMustMutate,
    #[serde(rename = "scope.workspace")]
    ScopeWorkspace,
    #[serde(rename = "scope.external")]
    ScopeExternal,
    #[serde(rename = "scope.mixed")]
    ScopeMixed,
    #[serde(rename = "scope.unknown")]
    ScopeUnknown,
    #[serde(rename = "domain.none")]
    DomainNone,
    #[serde(rename = "domain.github")]
    DomainGithub,
    #[serde(rename = "domain.git")]
    DomainGit,
    #[serde(rename = "domain.code")]
    DomainCode,
    #[serde(rename = "domain.memory")]
    DomainMemory,
    #[serde(rename = "domain.web")]
    DomainWeb,
    #[serde(rename = "domain.system")]
    DomainSystem,
    #[serde(rename = "domain.database")]
    DomainDatabase,
    #[serde(rename = "parallel_subruns")]
    ParallelSubruns,
    #[serde(rename = "capability.web")]
    CapabilityWeb,
    #[serde(rename = "delegation.model_requirement")]
    DelegationModelRequirement,
}
impl RequestJudgmentFieldV1 {
    pub fn from_question_id(id: &str) -> Option<Self> {
        match id {
            "required" => Some(Self::Required),
            "defer" => Some(Self::Defer),
            "mutation.read_only" => Some(Self::MutationReadOnly),
            "mutation.may_mutate" => Some(Self::MutationMayMutate),
            "mutation.must_mutate" => Some(Self::MutationMustMutate),
            "scope.workspace" => Some(Self::ScopeWorkspace),
            "scope.external" => Some(Self::ScopeExternal),
            "scope.mixed" => Some(Self::ScopeMixed),
            "scope.unknown" => Some(Self::ScopeUnknown),
            "domain.none" => Some(Self::DomainNone),
            "domain.github" => Some(Self::DomainGithub),
            "domain.git" => Some(Self::DomainGit),
            "domain.code" => Some(Self::DomainCode),
            "domain.memory" => Some(Self::DomainMemory),
            "domain.web" => Some(Self::DomainWeb),
            "domain.system" => Some(Self::DomainSystem),
            "domain.database" => Some(Self::DomainDatabase),
            "parallel_subruns" => Some(Self::ParallelSubruns),
            "capability.web" => Some(Self::CapabilityWeb),
            "delegation.model_requirement" => Some(Self::DelegationModelRequirement),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestJudgmentDomainV1 {
    Github,
    Git,
    Code,
    Memory,
    Web,
    System,
    Database,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestJudgmentMutationV1 {
    ReadOnly,
    MayMutate,
    MustMutate,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestJudgmentScopeV1 {
    Workspace,
    External,
    Mixed,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestJudgmentCapabilityV1 {
    Web,
    AgentSpawner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestJudgmentClassificationV1 {
    pub work_required: bool,
    pub activation_deferred: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    pub domain: Option<RequestJudgmentDomainV1>,
    pub mutation: RequestJudgmentMutationV1,
    pub scope: RequestJudgmentScopeV1,
    pub parallel_subruns: bool,
    pub capabilities: Vec<RequestJudgmentCapabilityV1>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestJudgmentFieldAssessmentV1 {
    pub field: RequestJudgmentFieldV1,
    pub score: SemanticJudgmentScoreV1,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestJudgmentAssessmentV1 {
    /// Discrete 0/0.5/1 values are categories, not calibrated confidence.
    pub provenance: JudgmentResponseProvenance,
    pub fields: Vec<RequestJudgmentFieldAssessmentV1>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentInvalidV1 {
    MalformedJson,
    InvalidContract,
    UnsupportedCombination,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentPreDispatchReasonV1 {
    NoOffering,
    CapacityPressure,
    InvalidRequest,
    OutputBudget,
    RouteUnavailable,
    DurableMaterialUnavailable,
    PreparationDeadline,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentUnavailableReasonV1 {
    ExecutionError,
    Deadline,
    Cancelled,
    ProviderPtlError,
    UnexpectedFinish,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticJudgmentDeliveryV1 {
    /// Entering summarize cannot establish whether a physical request ran.
    Unresolved,
    ResponseReceived,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestJudgmentResultV1 {
    Decided {
        classification: RequestJudgmentClassificationV1,
    },
    Abstained {
        uncertain_fields: Vec<RequestJudgmentFieldV1>,
        assessment: RequestJudgmentAssessmentV1,
    },
    Conflicting {
        fields: Vec<RequestJudgmentFieldV1>,
    },
    Invalid {
        reason: SemanticJudgmentInvalidV1,
    },
    NotDispatched {
        reason: SemanticJudgmentPreDispatchReasonV1,
    },
    Unavailable {
        reason: SemanticJudgmentUnavailableReasonV1,
        delivery: SemanticJudgmentDeliveryV1,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticJudgmentFactV1 {
    pub stage: RequestJudgmentStageV1,
    pub result: RequestJudgmentResultV1,
}
impl SemanticJudgmentFactV1 {
    /// Short, user-facing outcome for Explain's preparation timeline.
    /// Provider/model and usage belong to the correlated physical-attempt
    /// ledger; this label describes only the semantic outcome.
    pub fn preparation_label(&self) -> String {
        self.presentation_label()
    }

    /// Shared single-fact label for Explain and the bounded observation views.
    /// The stage is explicit so an initial result is not mistaken for a later
    /// clarification, and the wording remains descriptive rather than causal.
    pub fn presentation_label(&self) -> String {
        let stage = match self.stage {
            RequestJudgmentStageV1::Initial => "initial",
            RequestJudgmentStageV1::Clarification => "clarification",
        };
        bounded_presentation_label(format!(
            "Classify request · {stage} · {}",
            self.result.presentation_label()
        ))
    }
    pub const fn preparation_outcome(&self) -> crate::ExplainAnalyzeOutcomeV1 {
        use crate::ExplainAnalyzeOutcomeV1 as O;
        match &self.result {
            RequestJudgmentResultV1::Decided { .. } | RequestJudgmentResultV1::Abstained { .. } => {
                O::Completed
            }
            RequestJudgmentResultV1::Conflicting { .. }
            | RequestJudgmentResultV1::Invalid { .. } => O::Rejected,
            RequestJudgmentResultV1::NotDispatched {
                reason: SemanticJudgmentPreDispatchReasonV1::Cancelled,
            }
            | RequestJudgmentResultV1::Unavailable {
                reason: SemanticJudgmentUnavailableReasonV1::Cancelled,
                ..
            } => O::Cancelled,
            RequestJudgmentResultV1::Unavailable {
                reason: SemanticJudgmentUnavailableReasonV1::ExecutionError,
                ..
            } => O::Failed,
            RequestJudgmentResultV1::Unavailable {
                reason:
                    SemanticJudgmentUnavailableReasonV1::ProviderPtlError
                    | SemanticJudgmentUnavailableReasonV1::UnexpectedFinish,
                ..
            } => O::Rejected,
            _ => O::Unavailable,
        }
    }
}

fn bounded_presentation_label(label: String) -> String {
    if label.len() <= SEMANTIC_JUDGMENT_PRESENTATION_MAX_BYTES {
        return label;
    }
    let suffix = "...";
    let mut end = SEMANTIC_JUDGMENT_PRESENTATION_MAX_BYTES - suffix.len();
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(separator) = label[..end].rfind(" · ") {
        end = separator;
    }
    format!("{}{}", &label[..end], suffix)
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SemanticJudgmentInvocationV1 {
    /// Correlation only: a known invocation does not prove provider delivery.
    Known {
        invocation_id: String,
    },
    Unavailable,
}

impl<'de> Deserialize<'de> for SemanticJudgmentInvocationV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Serde's internally tagged unit variants ignore extra fields. An
        // empty struct variant closes that hole without changing the public API.
        #[derive(Deserialize)]
        #[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Known { invocation_id: String },
            Unavailable {},
        }
        match Wire::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom("invalid semantic invocation correlation"))?
        {
            Wire::Known { invocation_id } => Ok(Self::Known { invocation_id }),
            Wire::Unavailable {} => Ok(Self::Unavailable),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticJudgmentCorrelationV1 {
    pub run_id: String,
    pub turn: u32,
    pub round: u32,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    pub owner_generation: Option<u64>,
    /// One preflight identity shared by initial classification and clarification.
    pub evaluation_span_id: String,
    pub invocation: SemanticJudgmentInvocationV1,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SemanticJudgmentObservationV1 {
    pub schema_version: u16,
    pub correlation: SemanticJudgmentCorrelationV1,
    pub fact: SemanticJudgmentFactV1,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationWire {
    schema_version: u16,
    correlation: SemanticJudgmentCorrelationV1,
    fact: SemanticJudgmentFactV1,
}
impl<'de> Deserialize<'de> for SemanticJudgmentObservationV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ObservationWire::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom("invalid semantic judgment contract"))?;
        let observation = Self {
            schema_version: wire.schema_version,
            correlation: wire.correlation,
            fact: wire.fact,
        };
        observation.validate().map_err(serde::de::Error::custom)?;
        Ok(observation)
    }
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= SEMANTIC_JUDGMENT_ID_MAX_BYTES
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}
fn unique_fields(fields: &[RequestJudgmentFieldV1]) -> bool {
    !fields.is_empty()
        && fields.len() <= REQUEST_JUDGMENT_MAX_FIELDS
        && fields
            .iter()
            .enumerate()
            .all(|(i, f)| !fields[..i].contains(f))
}
impl RequestJudgmentResultV1 {
    /// A bounded, user-facing description of the typed result.
    ///
    /// This is a classification fact only.  In particular, words such as
    /// "required", "deferred", and "capability" do not mean that the
    /// corresponding work was started, paused, authorized, or executed.
    pub fn presentation_label(&self) -> String {
        self.presentation_label_with(false)
    }

    /// Complete fixed-field rendering for diagnostic surfaces.  The typed
    /// classification remains the source of truth; this is only a view.
    pub fn detail_label(&self) -> String {
        self.presentation_label_with(true)
    }

    fn presentation_label_with(&self, detailed: bool) -> String {
        match self {
            Self::Decided { classification } => {
                if detailed {
                    classification.detail_label()
                } else {
                    classification.presentation_label()
                }
            }
            Self::Abstained {
                uncertain_fields, ..
            } => format!("uncertain fields={}", presentation_fields(uncertain_fields)),
            Self::Conflicting { fields } => {
                format!("conflicting fields={}", presentation_fields(fields))
            }
            Self::Invalid { reason } => format!("invalid ({})", invalid_label(*reason)),
            Self::NotDispatched { reason } => {
                format!("not dispatched ({})", pre_dispatch_label(*reason))
            }
            Self::Unavailable { reason, delivery } => format!(
                "unavailable ({}, {})",
                unavailable_label(*reason),
                delivery_label(*delivery)
            ),
        }
    }

    pub fn validate(&self) -> Result<(), SemanticJudgmentValidationError> {
        let valid = match self {
            Self::Decided { classification: c } => {
                (!c.activation_deferred || c.work_required)
                    && (c.mutation == RequestJudgmentMutationV1::MustMutate
                        || c.scope == RequestJudgmentScopeV1::Unknown)
                    && c.capabilities.len() <= 2
                    && c.capabilities
                        .iter()
                        .enumerate()
                        .all(|(i, f)| !c.capabilities[..i].contains(f))
            }
            Self::Abstained {
                uncertain_fields,
                assessment,
            } => {
                if assessment.fields.len() > REQUEST_JUDGMENT_MAX_FIELDS {
                    return Err(SemanticJudgmentValidationError::InvalidContract);
                }
                let fields: Vec<_> = assessment.fields.iter().map(|f| f.field).collect();
                unique_fields(uncertain_fields)
                    && unique_fields(&fields)
                    && uncertain_fields.iter().all(|f| fields.contains(f))
                    && (assessment.provenance != JudgmentResponseProvenance::DiscreteDecision
                        || assessment.fields.iter().all(|f| f.score.is_discrete()))
            }
            Self::Conflicting { fields } => unique_fields(fields),
            Self::Unavailable { reason, delivery } => {
                matches!(
                    reason,
                    SemanticJudgmentUnavailableReasonV1::ProviderPtlError
                        | SemanticJudgmentUnavailableReasonV1::UnexpectedFinish
                ) == (*delivery == SemanticJudgmentDeliveryV1::ResponseReceived)
            }
            _ => true,
        };
        if valid {
            Ok(())
        } else {
            Err(SemanticJudgmentValidationError::InvalidContract)
        }
    }
}

impl RequestJudgmentClassificationV1 {
    /// Render the useful part of a structured classification without implying
    /// adoption.  This is intentionally compact for default summaries.
    pub fn presentation_label(&self) -> String {
        let work = if self.work_required {
            "Work required"
        } else {
            "Work not required"
        };
        let mut parts = vec![
            work.to_string(),
            mutation_display(self.mutation).to_string(),
        ];
        if self.activation_deferred {
            parts.push("activation deferred".into());
        }
        if let Some(domain) = self.domain {
            parts.push(format!("domain={}", domain_label(domain)));
        }
        parts.push(format!("scope={}", scope_label(self.scope)));
        if self.parallel_subruns {
            parts.push("parallel subruns".into());
        }
        if !self.capabilities.is_empty() {
            parts.push(format!(
                "capability={}",
                self.capabilities
                    .iter()
                    .map(|capability| capability_label(*capability))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        parts.join(" · ")
    }

    pub fn detail_label(&self) -> String {
        let work = if self.work_required {
            "required"
        } else {
            "not_required"
        };
        let activation = if self.activation_deferred {
            "deferred"
        } else {
            "not_deferred"
        };
        let domain = self.domain.map_or("none", domain_label);
        let capabilities = if self.capabilities.is_empty() {
            "none".to_string()
        } else {
            self.capabilities
                .iter()
                .map(|capability| capability_label(*capability))
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "work={work} · activation={activation} · domain={domain} · mutation={} · scope={} · topology={} · capabilities={capabilities}",
            mutation_label(self.mutation),
            scope_label(self.scope),
            if self.parallel_subruns {
                "parallel"
            } else {
                "single"
            },
        )
    }
}

fn presentation_fields(fields: &[RequestJudgmentFieldV1]) -> String {
    const MAX_RENDERED_FIELDS: usize = 3;
    let mut rendered = fields
        .iter()
        .take(MAX_RENDERED_FIELDS)
        .map(|field| field_label(*field))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if fields.len() > MAX_RENDERED_FIELDS {
        rendered.push(format!("+{} more", fields.len() - MAX_RENDERED_FIELDS));
    }
    if rendered.is_empty() {
        "none".into()
    } else {
        rendered.join(",")
    }
}

fn field_label(field: RequestJudgmentFieldV1) -> &'static str {
    match field {
        RequestJudgmentFieldV1::Required => "required",
        RequestJudgmentFieldV1::Defer => "defer",
        RequestJudgmentFieldV1::MutationReadOnly => "mutation.read_only",
        RequestJudgmentFieldV1::MutationMayMutate => "mutation.may_mutate",
        RequestJudgmentFieldV1::MutationMustMutate => "mutation.must_mutate",
        RequestJudgmentFieldV1::ScopeWorkspace => "scope.workspace",
        RequestJudgmentFieldV1::ScopeExternal => "scope.external",
        RequestJudgmentFieldV1::ScopeMixed => "scope.mixed",
        RequestJudgmentFieldV1::ScopeUnknown => "scope.unknown",
        RequestJudgmentFieldV1::DomainNone => "domain.none",
        RequestJudgmentFieldV1::DomainGithub => "domain.github",
        RequestJudgmentFieldV1::DomainGit => "domain.git",
        RequestJudgmentFieldV1::DomainCode => "domain.code",
        RequestJudgmentFieldV1::DomainMemory => "domain.memory",
        RequestJudgmentFieldV1::DomainWeb => "domain.web",
        RequestJudgmentFieldV1::DomainSystem => "domain.system",
        RequestJudgmentFieldV1::DomainDatabase => "domain.database",
        RequestJudgmentFieldV1::ParallelSubruns => "parallel_subruns",
        RequestJudgmentFieldV1::CapabilityWeb => "capability.web",
        RequestJudgmentFieldV1::DelegationModelRequirement => "delegation.model_requirement",
    }
}

fn domain_label(domain: RequestJudgmentDomainV1) -> &'static str {
    match domain {
        RequestJudgmentDomainV1::Github => "github",
        RequestJudgmentDomainV1::Git => "git",
        RequestJudgmentDomainV1::Code => "code",
        RequestJudgmentDomainV1::Memory => "memory",
        RequestJudgmentDomainV1::Web => "web",
        RequestJudgmentDomainV1::System => "system",
        RequestJudgmentDomainV1::Database => "database",
    }
}

fn mutation_label(mutation: RequestJudgmentMutationV1) -> &'static str {
    match mutation {
        RequestJudgmentMutationV1::ReadOnly => "read_only",
        RequestJudgmentMutationV1::MayMutate => "may_mutate",
        RequestJudgmentMutationV1::MustMutate => "must_mutate",
        RequestJudgmentMutationV1::Unknown => "unknown",
    }
}

fn mutation_display(mutation: RequestJudgmentMutationV1) -> &'static str {
    match mutation {
        RequestJudgmentMutationV1::ReadOnly => "read-only",
        RequestJudgmentMutationV1::MayMutate => "may mutate",
        RequestJudgmentMutationV1::MustMutate => "must mutate",
        RequestJudgmentMutationV1::Unknown => "mutation unknown",
    }
}

fn scope_label(scope: RequestJudgmentScopeV1) -> &'static str {
    match scope {
        RequestJudgmentScopeV1::Workspace => "workspace",
        RequestJudgmentScopeV1::External => "external",
        RequestJudgmentScopeV1::Mixed => "mixed",
        RequestJudgmentScopeV1::Unknown => "unknown",
    }
}

fn capability_label(capability: RequestJudgmentCapabilityV1) -> &'static str {
    match capability {
        RequestJudgmentCapabilityV1::Web => "web",
        RequestJudgmentCapabilityV1::AgentSpawner => "agent_spawner",
    }
}

fn invalid_label(reason: SemanticJudgmentInvalidV1) -> &'static str {
    match reason {
        SemanticJudgmentInvalidV1::MalformedJson => "malformed response",
        SemanticJudgmentInvalidV1::InvalidContract => "invalid contract",
        SemanticJudgmentInvalidV1::UnsupportedCombination => "unsupported combination",
    }
}

fn pre_dispatch_label(reason: SemanticJudgmentPreDispatchReasonV1) -> &'static str {
    match reason {
        SemanticJudgmentPreDispatchReasonV1::NoOffering => "no eligible model",
        SemanticJudgmentPreDispatchReasonV1::CapacityPressure => "capacity pressure",
        SemanticJudgmentPreDispatchReasonV1::InvalidRequest => "invalid request",
        SemanticJudgmentPreDispatchReasonV1::OutputBudget => "output budget",
        SemanticJudgmentPreDispatchReasonV1::RouteUnavailable => "route unavailable",
        SemanticJudgmentPreDispatchReasonV1::DurableMaterialUnavailable => {
            "durable material unavailable"
        }
        SemanticJudgmentPreDispatchReasonV1::PreparationDeadline => "preparation deadline",
        SemanticJudgmentPreDispatchReasonV1::Cancelled => "cancelled",
    }
}

fn unavailable_label(reason: SemanticJudgmentUnavailableReasonV1) -> &'static str {
    match reason {
        SemanticJudgmentUnavailableReasonV1::ExecutionError => "execution error",
        SemanticJudgmentUnavailableReasonV1::Deadline => "deadline",
        SemanticJudgmentUnavailableReasonV1::Cancelled => "cancelled",
        SemanticJudgmentUnavailableReasonV1::ProviderPtlError => "provider rejected",
        SemanticJudgmentUnavailableReasonV1::UnexpectedFinish => "unexpected finish",
    }
}

fn delivery_label(delivery: SemanticJudgmentDeliveryV1) -> &'static str {
    match delivery {
        SemanticJudgmentDeliveryV1::Unresolved => "delivery unresolved",
        SemanticJudgmentDeliveryV1::ResponseReceived => "response received",
    }
}

impl SemanticJudgmentObservationV1 {
    pub fn validate(&self) -> Result<(), SemanticJudgmentValidationError> {
        let c = &self.correlation;
        if self.schema_version != SEMANTIC_JUDGMENT_SCHEMA_VERSION
            || !valid_id(&c.run_id)
            || !valid_id(&c.evaluation_span_id)
            || !match &c.invocation {
                SemanticJudgmentInvocationV1::Known { invocation_id } => valid_id(invocation_id),
                SemanticJudgmentInvocationV1::Unavailable => true,
            }
        {
            return Err(SemanticJudgmentValidationError::InvalidContract);
        }
        self.fact.result.validate()
    }
    pub fn from_json(raw: &str) -> Result<Self, SemanticJudgmentValidationError> {
        if raw.len() > SEMANTIC_JUDGMENT_MAX_BYTES {
            return Err(SemanticJudgmentValidationError::Oversized);
        }
        serde_json::from_str(raw).map_err(|_| SemanticJudgmentValidationError::InvalidContract)
    }
    pub fn to_json(&self) -> Result<String, SemanticJudgmentValidationError> {
        self.validate()?;
        let raw = serde_json::to_string(self)
            .map_err(|_| SemanticJudgmentValidationError::InvalidContract)?;
        if raw.len() > SEMANTIC_JUDGMENT_MAX_BYTES {
            Err(SemanticJudgmentValidationError::Oversized)
        } else {
            Ok(raw)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn observation() -> SemanticJudgmentObservationV1 {
        SemanticJudgmentObservationV1 {
            schema_version: 1,
            correlation: SemanticJudgmentCorrelationV1 {
                run_id: "run-1".into(),
                turn: 0,
                round: 2,
                owner_generation: Some(7),
                evaluation_span_id: "preflight-1".into(),
                invocation: SemanticJudgmentInvocationV1::Unavailable,
            },
            fact: SemanticJudgmentFactV1 {
                stage: RequestJudgmentStageV1::Initial,
                result: RequestJudgmentResultV1::Abstained {
                    uncertain_fields: vec![RequestJudgmentFieldV1::Required],
                    assessment: RequestJudgmentAssessmentV1 {
                        provenance: JudgmentResponseProvenance::ProviderProbability,
                        fields: vec![RequestJudgmentFieldAssessmentV1 {
                            field: RequestJudgmentFieldV1::Required,
                            score: SemanticJudgmentScoreV1::from_f64(0.512_345_678_901_234_5)
                                .unwrap(),
                        }],
                    },
                },
            },
        }
    }
    fn reject(value: Value) {
        let raw = value.to_string();
        assert_eq!(
            SemanticJudgmentObservationV1::from_json(&raw),
            Err(SemanticJudgmentValidationError::InvalidContract)
        );
        let error = serde_json::from_str::<SemanticJudgmentObservationV1>(&raw).unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_PAYLOAD"));
    }
    #[test]
    fn request_judgment_score_preserves_precision_and_eq() {
        fn eq<T: Eq>() {}
        eq::<SemanticJudgmentObservationV1>();
        for value in [0.0, 0.5, 1.0, 0.812_345_678_901_234_5, f64::MIN_POSITIVE] {
            let score = SemanticJudgmentScoreV1::from_f64(value).unwrap();
            let restored: SemanticJudgmentScoreV1 =
                serde_json::from_str(&serde_json::to_string(&score).unwrap()).unwrap();
            assert_eq!(score, restored);
            assert_eq!(restored.as_number().as_f64(), Some(value));
        }
        for value in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
            assert!(SemanticJudgmentScoreV1::from_f64(value).is_err());
        }
    }
    #[test]
    fn request_judgment_stages_and_results_roundtrip() {
        let results = vec![
            observation().fact.result,
            RequestJudgmentResultV1::Decided {
                classification: RequestJudgmentClassificationV1 {
                    work_required: false,
                    activation_deferred: false,
                    domain: Some(RequestJudgmentDomainV1::Memory),
                    mutation: RequestJudgmentMutationV1::MustMutate,
                    scope: RequestJudgmentScopeV1::External,
                    parallel_subruns: false,
                    capabilities: vec![],
                },
            },
            RequestJudgmentResultV1::Conflicting {
                fields: vec![RequestJudgmentFieldV1::Required],
            },
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::InvalidContract,
            },
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::MalformedJson,
            },
            RequestJudgmentResultV1::Invalid {
                reason: SemanticJudgmentInvalidV1::UnsupportedCombination,
            },
            RequestJudgmentResultV1::NotDispatched {
                reason: SemanticJudgmentPreDispatchReasonV1::NoOffering,
            },
            RequestJudgmentResultV1::Unavailable {
                reason: SemanticJudgmentUnavailableReasonV1::Deadline,
                delivery: SemanticJudgmentDeliveryV1::Unresolved,
            },
            RequestJudgmentResultV1::Unavailable {
                reason: SemanticJudgmentUnavailableReasonV1::UnexpectedFinish,
                delivery: SemanticJudgmentDeliveryV1::ResponseReceived,
            },
        ];
        for stage in [
            RequestJudgmentStageV1::Initial,
            RequestJudgmentStageV1::Clarification,
        ] {
            for result in &results {
                let mut fact = observation();
                fact.fact = SemanticJudgmentFactV1 {
                    stage,
                    result: result.clone(),
                };
                let raw = fact.to_json().unwrap();
                assert_eq!(
                    SemanticJudgmentObservationV1::from_json(&raw).unwrap(),
                    fact
                );
                assert!(
                    fact.fact
                        .preparation_label()
                        .starts_with("Classify request ·")
                );
                assert!(fact.fact.preparation_label().len() < 160);
                let value: Value = serde_json::from_str(&raw).unwrap();
                for key in ["usage", "evidence", "model_adoption"] {
                    assert!(value.get(key).is_none());
                }
                assert!(value["correlation"].get("snapshot_key").is_none());
                assert_eq!(
                    value["correlation"]["invocation"],
                    json!({"status":"unavailable"})
                );
            }
        }
    }

    #[test]
    fn uncertain_initial_stage_does_not_claim_fallback_before_clarification() {
        let mut initial = observation();
        assert!(matches!(
            initial.fact.result,
            RequestJudgmentResultV1::Abstained { .. }
        ));
        assert_eq!(
            initial.fact.preparation_label(),
            "Classify request · initial · uncertain fields=required"
        );
        assert!(!initial.fact.preparation_label().contains("baseline"));

        initial.fact.stage = RequestJudgmentStageV1::Clarification;
        initial.fact.result = RequestJudgmentResultV1::Decided {
            classification: RequestJudgmentClassificationV1 {
                work_required: true,
                activation_deferred: false,
                domain: Some(RequestJudgmentDomainV1::Code),
                mutation: RequestJudgmentMutationV1::MustMutate,
                scope: RequestJudgmentScopeV1::Workspace,
                parallel_subruns: false,
                capabilities: vec![],
            },
        };
        assert_eq!(
            initial.fact.preparation_label(),
            "Classify request · clarification · Work required · must mutate · domain=code · scope=workspace"
        );
    }
    #[test]
    fn request_judgment_rejects_raw_data_and_obsolete_contracts() {
        let original = serde_json::to_value(observation()).unwrap();
        for pointer in [
            "",
            "/correlation",
            "/correlation/invocation",
            "/fact",
            "/fact/result",
            "/fact/result/assessment",
            "/fact/result/assessment/fields/0",
        ] {
            let mut value = original.clone();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("raw".into(), json!("PRIVATE_PAYLOAD"));
            reject(value);
        }
        for (pointer, replacement) in [
            ("/schema_version", json!(2)),
            ("/fact/stage", json!("work_direction_evaluated")),
            ("/fact/result/uncertain_fields", json!(["PRIVATE_PAYLOAD"])),
            ("/fact/result/assessment/fields/0/score", json!(1.1)),
            ("/correlation/run_id", json!("PRIVATE_PAYLOAD\n")),
        ] {
            let mut value = original.clone();
            *value.pointer_mut(pointer).unwrap() = replacement;
            reject(value);
        }
        let mut old = original.clone();
        old["evidence"] = json!({"omitted_observations":false,"support_basis_available":true});
        reject(old);
        for key in ["owner_generation", "invocation"] {
            let mut value = original.clone();
            value["correlation"].as_object_mut().unwrap().remove(key);
            reject(value);
        }
    }
    #[test]
    fn request_judgment_rejects_duplicate_missing_and_inconsistent_evidence() {
        let mut value = serde_json::to_value(observation()).unwrap();
        value["fact"]["result"]["uncertain_fields"] = json!(["required", "required"]);
        reject(value);
        let mut value = serde_json::to_value(observation()).unwrap();
        value["fact"]["result"]["assessment"]["fields"] = json!([]);
        reject(value);
        let mut value = serde_json::to_value(observation()).unwrap();
        value["fact"]["result"]["assessment"]["provenance"] = json!("discrete_decision");
        reject(value.clone());
        value["fact"]["result"]["assessment"]["fields"][0]["score"] = json!(0.5);
        assert!(SemanticJudgmentObservationV1::from_json(&value.to_string()).is_ok());
        let mut fact = observation();
        fact.fact.result = RequestJudgmentResultV1::Unavailable {
            reason: SemanticJudgmentUnavailableReasonV1::Deadline,
            delivery: SemanticJudgmentDeliveryV1::ResponseReceived,
        };
        assert!(fact.to_json().is_err());
    }
    #[test]
    fn request_judgment_bounded_ids_and_bytes() {
        let mut fact = observation();
        fact.correlation.run_id = "r".repeat(SEMANTIC_JUDGMENT_ID_MAX_BYTES);
        fact.correlation.evaluation_span_id = "e".repeat(SEMANTIC_JUDGMENT_ID_MAX_BYTES);
        assert!(fact.to_json().is_ok());
        fact.correlation.run_id.push('x');
        assert!(fact.to_json().is_err());
        assert_eq!(
            SemanticJudgmentObservationV1::from_json(&" ".repeat(SEMANTIC_JUDGMENT_MAX_BYTES + 1)),
            Err(SemanticJudgmentValidationError::Oversized)
        );
    }
    #[test]
    fn request_judgment_presentation_does_not_claim_execution_or_usage() {
        use crate::ExplainAnalyzeOutcomeV1 as Outcome;
        assert_eq!(observation().fact.preparation_outcome(), Outcome::Completed);
        let decided = RequestJudgmentResultV1::Decided {
            classification: RequestJudgmentClassificationV1 {
                work_required: true,
                activation_deferred: true,
                domain: Some(RequestJudgmentDomainV1::Code),
                mutation: RequestJudgmentMutationV1::MayMutate,
                scope: RequestJudgmentScopeV1::Workspace,
                parallel_subruns: true,
                capabilities: vec![RequestJudgmentCapabilityV1::Web],
            },
        };
        let label = decided.presentation_label();
        assert!(label.contains("Work required"));
        assert!(label.contains("activation deferred"));
        assert!(label.contains("domain=code"));
        assert!(label.contains("may mutate"));
        assert!(label.contains("scope=workspace"));
        assert!(label.contains("parallel subruns"));
        assert!(label.contains("capability=web"));
        assert!(!label.contains("executed") && !label.contains("authorized"));
        let mut fact = observation().fact;
        fact.result = RequestJudgmentResultV1::Conflicting {
            fields: vec![RequestJudgmentFieldV1::Required],
        };
        assert_eq!(fact.preparation_outcome(), Outcome::Rejected);
        fact.result = RequestJudgmentResultV1::NotDispatched {
            reason: SemanticJudgmentPreDispatchReasonV1::Cancelled,
        };
        assert_eq!(fact.preparation_outcome(), Outcome::Cancelled);
    }
}
