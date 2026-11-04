//! Lazy provider contracts, with custody in the canonical run event store.
//!
//! A source reference is discovery evidence, never a grant. Descriptors pass
//! through the ordinary provider resolver; invocation retains ordinary
//! admission, permission and durable dispatch. No product vocabulary belongs
//! in this module.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use astra_services::runs::RunStateStore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

pub(crate) const CONTRACT_VERSION: &str = "astra-capability-source-v1";
pub(crate) const CONFIG_METADATA_KEY: &str = "astra.capability_source";
pub(crate) const CANDIDATES_METADATA_KEY: &str = "astra.capability_candidates";
const CANDIDATE_EVENT: &str = "capability_source_candidate";
const DESCRIPTOR_EVENT: &str = "capability_source_descriptor";
const MAX_PAGE: usize = 8;
const MAX_DESCRIPTOR_BYTES: usize = 64 * 1024;
const MAX_SELECTION_LABEL_BYTES: usize = 24;

// Unlike MCP display-name sanitization, selection names must be bounded ASCII
// for every tool provider. Encode whole codepoints; never split an escape.
fn selection_label(native_id: &str) -> String {
    let mut label = String::new();
    for ch in native_id.chars() {
        let part = if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            ch.to_ascii_lowercase().to_string()
        } else if ch.is_ascii() {
            "_".to_string()
        } else {
            format!("u{:x}", u32::from(ch))
        };
        if label.len() + part.len() > MAX_SELECTION_LABEL_BYTES {
            break;
        }
        label.push_str(&part);
    }
    label
}

pub(super) fn valid_selection_key(key: &str) -> bool {
    ["mcp__cap__", "skill__cap__"].iter().any(|prefix| {
        key.strip_prefix(prefix)
            .and_then(|suffix| suffix.rsplit_once("__"))
            .is_some_and(|(label, digest)| {
                !label.is_empty()
                    && label.len() <= MAX_SELECTION_LABEL_BYTES
                    && label.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-'
                    })
                    && digest.len() == 16
                    && digest
                        .bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            })
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilitySourceConfig {
    pub contract_version: String,
    pub discovery_tools: BTreeSet<String>,
    /// Explicitly delegates selected calls to the admitted discovery scope.
    pub selection_authorization: String,
}

impl CapabilitySourceConfig {
    pub(crate) fn parse(
        metadata: &serde_json::Map<String, Value>,
    ) -> Result<Option<Self>, SourceError> {
        let Some(raw) = metadata.get(CONFIG_METADATA_KEY) else {
            return Ok(None);
        };
        let config: Self =
            serde_json::from_value(raw.clone()).map_err(|_| SourceError::InvalidContract)?;
        config.validate()?;
        Ok(Some(config))
    }

    pub(crate) fn validate(&self) -> Result<(), SourceError> {
        if self.contract_version != CONTRACT_VERSION
            || self.selection_authorization != "discovery_scope"
            || self.discovery_tools.is_empty()
            || self.discovery_tools.len() > MAX_PAGE
            || self.discovery_tools.iter().any(|id| !valid_id(id, 256))
        {
            return Err(SourceError::InvalidContract);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityKind {
    Tool,
    Skill,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilityReference {
    pub snapshot_revision: String,
    pub binding_id: String,
    pub native_id: String,
    pub descriptor_digest: String,
    pub reference: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilityCandidate {
    pub kind: CapabilityKind,
    pub title: String,
    pub summary: String,
    pub capability: CapabilityReference,
}

impl CapabilityCandidate {
    fn validate(&self) -> Result<(), SourceError> {
        let r = &self.capability;
        if !valid_id(&r.snapshot_revision, 256)
            || !valid_id(&r.binding_id, 256)
            || !valid_id(&r.native_id, 256)
            || !valid_id(&r.reference, 2048)
            || !valid_digest(&r.descriptor_digest)
            || !valid_id(&self.title, 128)
            || self.summary.len() > 512
            || self.summary.contains('\0')
        {
            return Err(SourceError::InvalidContract);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilityCandidatePage {
    pub contract_version: String,
    pub candidates: Vec<CapabilityCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CapabilitySelection {
    pub selection_key: String,
    pub kind: CapabilityKind,
    pub title: String,
    pub summary: String,
    pub activation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceScope {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub generation: u64,
}

/// Persisted receipts contain no endpoint credentials. Source identity includes
/// the authenticated descriptor configuration, not a provider-authored URL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateReceipt {
    source_id: String,
    discovery_tool: String,
    config_digest: String,
    user_id: String,
    session_id: String,
    run_id: String,
    candidate: CapabilityCandidate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DescriptorReceipt {
    candidate: CandidateReceipt,
    descriptor_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SourceError {
    #[error("capability_source_contract_invalid")]
    InvalidContract,
    #[error("capability_source_selection_not_found")]
    SelectionNotFound,
    #[error("capability_source_selection_changed")]
    SelectionChanged,
    #[error("capability_source_custody_unavailable")]
    CustodyUnavailable,
    #[error("capability_source_run_superseded")]
    RunSuperseded,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceInvocationError {
    #[error(transparent)]
    Rpc(#[from] super::runtime_mcp::AgentBindingMcpRpcError),
    #[error("capability_source_admission_deadline_expired")]
    AdmissionDeadlineExpired,
    #[error("capability_source_cancelled_before_dispatch")]
    CancelledBeforeDispatch,
}

pub(crate) struct CapabilitySourceCustody {
    source_id: String,
    config: CapabilitySourceConfig,
    config_digest: String,
    store: Arc<dyn RunStateStore>,
}

impl CapabilitySourceCustody {
    pub(crate) fn new(
        source_id: &str,
        config: CapabilitySourceConfig,
        store: Arc<dyn RunStateStore>,
    ) -> Self {
        let config_digest =
            content_digest(&serde_json::to_value(&config).expect("source config serializes"));
        Self {
            source_id: source_id.to_string(),
            config,
            config_digest,
            store,
        }
    }

    pub(crate) fn is_discovery_tool(&self, native_id: &str) -> bool {
        self.config.discovery_tools.contains(native_id)
    }

    pub(crate) fn owns_selection_shape(&self, key: &str) -> bool {
        // Reserve the namespace even for malformed names. A typo must not
        // change execution owner by falling through to a local skill/tool.
        ["mcp__cap__", "skill__cap__"].iter().any(|prefix| {
            key.get(..prefix.len())
                .is_some_and(|actual| actual.eq_ignore_ascii_case(prefix))
        })
    }

    fn receipt(
        &self,
        scope: &SourceScope,
        discovery_tool: &str,
        candidate: CapabilityCandidate,
    ) -> Result<CandidateReceipt, SourceError> {
        if !self.is_discovery_tool(discovery_tool) {
            return Err(SourceError::InvalidContract);
        }
        candidate.validate()?;
        if [&scope.user_id, &scope.session_id, &scope.run_id]
            .iter()
            .any(|id| !valid_id(id, 256))
        {
            return Err(SourceError::InvalidContract);
        }
        Ok(CandidateReceipt {
            source_id: self.source_id.clone(),
            discovery_tool: discovery_tool.into(),
            config_digest: self.config_digest.clone(),
            user_id: scope.user_id.clone(),
            session_id: scope.session_id.clone(),
            run_id: scope.run_id.clone(),
            candidate,
        })
    }

    fn selection_key(&self, receipt: &CandidateReceipt) -> String {
        let digest = content_digest(&json!([
            receipt.source_id,
            receipt.discovery_tool,
            receipt.config_digest,
            receipt.candidate.kind,
            receipt.candidate.capability
        ]));
        format!(
            "{}__cap__{}__{}",
            match receipt.candidate.kind {
                CapabilityKind::Tool => "mcp",
                CapabilityKind::Skill => "skill",
            },
            selection_label(&receipt.candidate.capability.native_id),
            // A stable short disambiguator is not authority. Registration
            // rejects conflicting full receipts; resolution still requires
            // this run's exact receipt and complete descriptor/reference.
            &digest[7..23]
        )
    }

    /// Validate the complete page before committing any sibling. Immutable
    /// indexed receipts survive lost responses and checkpoint/result replay.
    pub(crate) async fn register(
        &self,
        scope: &SourceScope,
        native_tool: &str,
        page: CapabilityCandidatePage,
    ) -> Result<Vec<CapabilitySelection>, SourceError> {
        if !self.is_discovery_tool(native_tool)
            || page.contract_version != CONTRACT_VERSION
            || page.candidates.len() > MAX_PAGE
        {
            return Err(SourceError::InvalidContract);
        }
        let mut receipts = BTreeMap::new();
        let mut selections = Vec::new();
        for candidate in page.candidates {
            let receipt = self.receipt(scope, native_tool, candidate)?;
            let key = self.selection_key(&receipt);
            if receipts.insert(key.clone(), receipt.clone()).is_some() {
                return Err(SourceError::InvalidContract);
            }
            let activation = match receipt.candidate.kind {
                CapabilityKind::Tool => format!("tool_search(select:{key})"),
                CapabilityKind::Skill => format!("skill(skill_name:\"{key}\")"),
            };
            selections.push(CapabilitySelection {
                selection_key: key,
                kind: receipt.candidate.kind,
                title: receipt.candidate.title.clone(),
                summary: receipt.candidate.summary.clone(),
                activation,
            });
        }
        let events = receipts
            .into_iter()
            .map(|(key, receipt)| receipt_event(CANDIDATE_EVENT, &key, &receipt))
            .collect::<Vec<_>>();
        self.commit(scope, &events).await?;
        Ok(selections)
    }

    #[cfg(test)]
    async fn candidate(
        &self,
        scope: &SourceScope,
        key: &str,
    ) -> Result<CapabilityCandidate, SourceError> {
        Ok(self.candidate_receipt(scope, key).await?.candidate)
    }

    pub(crate) async fn selection_kind(
        &self,
        scope: &SourceScope,
        key: &str,
    ) -> Result<CapabilityKind, SourceError> {
        Ok(self.candidate_receipt(scope, key).await?.candidate.kind)
    }

    async fn candidate_receipt(
        &self,
        scope: &SourceScope,
        key: &str,
    ) -> Result<CandidateReceipt, SourceError> {
        if !valid_selection_key(key) {
            return Err(SourceError::SelectionNotFound);
        }
        let value = self
            .load(scope, CANDIDATE_EVENT, key)
            .await?
            .ok_or(SourceError::SelectionNotFound)?;
        let receipt: CandidateReceipt =
            serde_json::from_value(value).map_err(|_| SourceError::InvalidContract)?;
        if receipt.source_id != self.source_id || receipt.config_digest != self.config_digest {
            return Err(SourceError::SelectionChanged);
        }
        let expected = self.receipt(scope, &receipt.discovery_tool, receipt.candidate.clone())?;
        if receipt != expected || self.selection_key(&receipt) != key {
            return Err(SourceError::SelectionChanged);
        }
        Ok(receipt)
    }

    pub(crate) async fn descriptor(
        &self,
        scope: &SourceScope,
        key: &str,
    ) -> Result<Option<(CapabilityCandidate, Value)>, SourceError> {
        let candidate_receipt = self.candidate_receipt(scope, key).await?;
        let candidate = candidate_receipt.candidate.clone();
        let Some(value) = self.load(scope, DESCRIPTOR_EVENT, key).await? else {
            return Ok(None);
        };
        let receipt: DescriptorReceipt =
            serde_json::from_value(value).map_err(|_| SourceError::InvalidContract)?;
        if receipt.candidate != candidate_receipt {
            return Err(SourceError::SelectionChanged);
        }
        let descriptor = validate_descriptor(&candidate, &receipt.descriptor_json)?;
        Ok(Some((candidate, descriptor)))
    }

    pub(crate) async fn record_descriptor(
        &self,
        scope: &SourceScope,
        key: &str,
        candidate: CapabilityCandidate,
        descriptor_json: String,
    ) -> Result<Value, SourceError> {
        let descriptor = validate_descriptor(&candidate, &descriptor_json)?;
        let candidate_receipt = self.candidate_receipt(scope, key).await?;
        if candidate_receipt.candidate != candidate {
            return Err(SourceError::SelectionChanged);
        }
        let receipt = DescriptorReceipt {
            candidate: candidate_receipt,
            descriptor_json,
        };
        if self.selection_key(&receipt.candidate) != key {
            return Err(SourceError::SelectionChanged);
        }
        self.commit(scope, &[receipt_event(DESCRIPTOR_EVENT, key, &receipt)])
            .await?;
        Ok(descriptor)
    }

    async fn load(
        &self,
        scope: &SourceScope,
        kind: &str,
        key: &str,
    ) -> Result<Option<Value>, SourceError> {
        let event = self
            .store
            .load_run_event_by_idempotency_key(
                &scope.user_id,
                &scope.run_id,
                kind,
                &format!("{kind}:{key}"),
            )
            .await
            .map_err(|error| storage_failure(scope, "load", error))?;
        Ok(event.map(|event| event["data"].clone()))
    }

    async fn commit(&self, scope: &SourceScope, events: &[Value]) -> Result<(), SourceError> {
        if events.is_empty() {
            return Ok(());
        }
        // Reject an immutable-key conflict before writing sibling receipts.
        // The store still arbitrates concurrent writers atomically below.
        for event in events {
            let kind = event["event_type"]
                .as_str()
                .ok_or(SourceError::InvalidContract)?;
            let key = event["idempotency_key"]
                .as_str()
                .ok_or(SourceError::InvalidContract)?;
            if let Some(stored) = self
                .store
                .load_run_event_by_idempotency_key(&scope.user_id, &scope.run_id, kind, key)
                .await
                .map_err(|error| storage_failure(scope, "check_commit", error))?
            {
                if stored["data"] != event["data"] {
                    return Err(SourceError::SelectionChanged);
                }
            }
        }
        let committed = self
            .store
            .append_events_if_current_generation_and_status(
                &scope.user_id,
                &scope.session_id,
                &scope.run_id,
                scope.generation,
                &["running"],
                events,
            )
            .await
            .map_err(|error| storage_failure(scope, "commit", error))?;
        if !committed {
            return Err(SourceError::RunSuperseded);
        }
        // An existing idempotency key is not proof of equal immutable bytes.
        for event in events {
            let kind = event["event_type"]
                .as_str()
                .ok_or(SourceError::InvalidContract)?;
            let key = event["idempotency_key"]
                .as_str()
                .ok_or(SourceError::InvalidContract)?;
            let stored = self
                .store
                .load_run_event_by_idempotency_key(&scope.user_id, &scope.run_id, kind, key)
                .await
                .map_err(|error| storage_failure(scope, "verify_commit", error))?
                .ok_or(SourceError::CustodyUnavailable)?;
            if stored["data"] != event["data"] {
                return Err(SourceError::SelectionChanged);
            }
        }
        Ok(())
    }
}

fn storage_failure(scope: &SourceScope, operation: &str, error: String) -> SourceError {
    tracing::error!(component = "capability_source", operation, reason = "custody_unavailable",
        user_id = scope.user_id, session_id = scope.session_id, run_id = scope.run_id,
        owner_generation = scope.generation, error = %super::runtime_mcp::redact_mcp_error_text(&error),
        "capability source custody failed");
    SourceError::CustodyUnavailable
}

fn receipt_event<T: Serialize>(kind: &str, key: &str, receipt: &T) -> Value {
    json!({"event_type": kind, "idempotency_key": format!("{kind}:{key}"), "data": receipt})
}

fn validate_descriptor(
    candidate: &CapabilityCandidate,
    encoded: &str,
) -> Result<Value, SourceError> {
    candidate.validate()?;
    if encoded.len() > MAX_DESCRIPTOR_BYTES
        || format!("sha256:{:x}", Sha256::digest(encoded.as_bytes()))
            != candidate.capability.descriptor_digest
    {
        return Err(SourceError::InvalidContract);
    }
    let value = astra_turn_types::parse_unique_json(encoded.as_bytes())
        .map_err(|_| SourceError::InvalidContract)?;
    if value.get("name").and_then(Value::as_str) != Some(candidate.capability.native_id.as_str()) {
        return Err(SourceError::InvalidContract);
    }
    match candidate.kind {
        CapabilityKind::Tool if !value.get("inputSchema").is_some_and(Value::is_object) => {
            return Err(SourceError::InvalidContract);
        }
        CapabilityKind::Skill => {
            let skill: SourceSkillContract =
                serde_json::from_value(value.clone()).map_err(|_| SourceError::InvalidContract)?;
            if skill.instructions.is_empty() || skill.instructions.contains('\0') {
                return Err(SourceError::InvalidContract);
            }
        }
        _ => {}
    }
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceSkillContract {
    name: String,
    instructions: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
    input_schema: Option<Value>,
    output_schema: Option<Value>,
}

fn valid_id(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    })
}

fn content_digest(value: &Value) -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(astra_core::canonical_json_string(value).as_bytes())
    )
}

#[derive(Clone)]
pub(crate) struct MaterializedCapability {
    pub candidate: CapabilityCandidate,
    pub discovery_tool: String,
    pub snapshot: astra_turn_types::ResolvedProviderSnapshot,
    pub schema: Value,
}

/// The cache is only a bounded projection of indexed canonical receipts. It
/// cannot register candidates, infer grants, or repair missing custody.
pub(crate) struct CapabilitySourceRuntime {
    pub custody: CapabilitySourceCustody,
    transport: Arc<super::runtime_mcp::AgentBindingMcpRuntime>,
    scope: std::sync::OnceLock<SourceScope>,
    cancel: std::sync::OnceLock<CancellationToken>,
    materialized: std::sync::RwLock<BTreeMap<String, MaterializedCapability>>,
    resolution: tokio::sync::Mutex<()>,
}

impl CapabilitySourceRuntime {
    /// Registered descriptors own their exact aliases; the reserved namespace
    /// rejects only otherwise-unbound selections, including malformed keys.
    pub(crate) fn owns_selection_shape(&self, name: &str) -> bool {
        self.custody.owns_selection_shape(name) && !self.transport.owns_public_tool_name(name)
    }

    pub(crate) fn new(
        transport: Arc<super::runtime_mcp::AgentBindingMcpRuntime>,
        store: Arc<dyn RunStateStore>,
    ) -> Option<Self> {
        let config = transport.capability_source_config.clone()?;
        Some(Self {
            custody: CapabilitySourceCustody::new(transport.server_name(), config, store),
            transport,
            scope: std::sync::OnceLock::new(),
            cancel: std::sync::OnceLock::new(),
            materialized: std::sync::RwLock::new(BTreeMap::new()),
            resolution: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn bind_scope(
        &self,
        scope: SourceScope,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), SourceError> {
        if let Some(current) = self.scope.get() {
            if current != &scope {
                return Err(SourceError::RunSuperseded);
            }
        } else {
            self.scope
                .set(scope)
                .map_err(|_| SourceError::RunSuperseded)?;
        }
        // The runtime is scoped to one run generation. Skill resolvers share
        // this owner, including when no ToolExecutionRequest is involved.
        if let Some(cancel) = cancel {
            self.cancel.get_or_init(|| cancel.clone());
        }
        Ok(())
    }

    pub(crate) fn request_scope(
        &self,
        request: &super::tool_execution_binding::ToolExecutionRequest,
    ) -> Result<&SourceScope, SourceError> {
        let scope = self.scope.get().ok_or(SourceError::CustodyUnavailable)?;
        if request.user_id != scope.user_id
            || request.session_id != scope.session_id
            || request.run_id != scope.run_id
        {
            return Err(SourceError::RunSuperseded);
        }
        Ok(scope)
    }

    pub(crate) async fn register_result(
        &self,
        request: &super::tool_execution_binding::ToolExecutionRequest,
        result: &mut astra_mcp::McpToolCallResult,
    ) -> Result<(), SourceError> {
        let Some(native) = self.transport.native_tool_name(&request.tool_name) else {
            return Ok(());
        };
        if !self.custody.is_discovery_tool(native) {
            return Ok(());
        }
        if result.is_error {
            return Ok(());
        }
        let Some(raw) = result
            .protocol_metadata
            .as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|metadata| metadata.remove(CANDIDATES_METADATA_KEY))
        else {
            return Ok(());
        };
        let page = serde_json::from_value(raw).map_err(|_| SourceError::InvalidContract)?;
        let selections = self
            .custody
            .register(self.request_scope(request)?, native, page)
            .await?;
        // A single runtime-owned projection is used for both model and client.
        // Opaque provider references never enter the visible tool result.
        let structured =
            json!({"source_output": result.structured_content.take(), "capabilities": selections});
        result.output =
            json!({"source_output": result.output, "capabilities": structured["capabilities"]})
                .to_string();
        result.structured_content = Some(structured);
        Ok(())
    }

    pub(crate) async fn materialize(
        &self,
        key: &str,
        allow_describe: bool,
    ) -> Result<MaterializedCapability, SourceError> {
        let scope = self.scope.get().ok_or(SourceError::CustodyUnavailable)?;
        if let Some(value) = self.cached(key)? {
            return Ok(value);
        }
        // Serialize remote resolution without holding the synchronous cache
        // lock across network/storage awaits. Readiness remains independent
        // of another selection's in-flight describe request.
        let _resolution = self.resolution.lock().await;
        if let Some(value) = self.cached(key)? {
            return Ok(value.clone());
        }
        if self
            .materialized
            .read()
            .map_err(|_| SourceError::CustodyUnavailable)?
            .len()
            >= 256
        {
            return Err(SourceError::InvalidContract);
        }
        let (candidate, raw_tool) = match self.custody.descriptor(scope, key).await? {
            Some(value) => value,
            None if allow_describe => {
                let receipt = self.custody.candidate_receipt(scope, key).await?;
                let candidate = receipt.candidate;
                let response = self
                    .transport
                    .capability_rpc(
                        "astra/capabilities/describe",
                        json!({"capability": candidate.capability, "discovery_tool": receipt.discovery_tool}),
                        false,
                        None,
                    )
                    .await
                    .map_err(|error| transport_failure(scope, "describe", &error))?;
                if response.get("capability")
                    != Some(
                        &serde_json::to_value(&candidate.capability).expect("reference serializes"),
                    )
                {
                    return Err(SourceError::SelectionChanged);
                }
                let encoded = response
                    .get("descriptor_json")
                    .and_then(Value::as_str)
                    .ok_or(SourceError::InvalidContract)?
                    .to_string();
                let raw = self
                    .custody
                    .record_descriptor(scope, key, candidate.clone(), encoded)
                    .await?;
                (candidate, raw)
            }
            None => return Err(SourceError::SelectionNotFound),
        };
        if candidate.kind != CapabilityKind::Tool {
            return Err(SourceError::InvalidContract);
        }
        let (snapshot, schema) = super::runtime_mcp::resolve_capability_source_tool(
            &self.custody.source_id,
            key,
            &candidate,
            raw_tool,
        )
        .map_err(|error| {
            storage_failure(scope, "resolve_descriptor", error);
            SourceError::InvalidContract
        })?;
        let receipt = self.custody.candidate_receipt(scope, key).await?;
        let discovery_tool = self
            .transport
            .public_tool_name(&receipt.discovery_tool)
            .ok_or(SourceError::InvalidContract)?
            .to_string();
        let mut cache = self
            .materialized
            .write()
            .map_err(|_| SourceError::CustodyUnavailable)?;
        let retained_bytes = cache
            .values()
            .map(|entry| entry.schema.to_string().len())
            .sum::<usize>();
        if retained_bytes + schema.to_string().len() > 512 * 1024 {
            return Err(SourceError::InvalidContract);
        }
        let result = MaterializedCapability {
            candidate,
            discovery_tool,
            snapshot,
            schema,
        };
        cache.insert(key.to_string(), result.clone());
        Ok(result)
    }

    pub(crate) fn selection_discovery_tool(
        &self,
        key: &str,
    ) -> Option<(String, astra_turn_types::ResolvedToolDescriptorRef)> {
        let alias = astra_turn_types::PublicToolAlias::new(key).ok()?;
        let cache = self.materialized.read().ok()?;
        let materialized = cache.get(key)?;
        let descriptor = materialized.snapshot.alias_index.get(&alias)?.clone();
        Some((materialized.discovery_tool.clone(), descriptor))
    }

    pub(crate) async fn invoke(
        &self,
        request: &super::tool_execution_binding::ToolExecutionRequest,
        interaction: Option<&astra_turn_types::ProviderInteractionResponse>,
        cancel_token: Option<&CancellationToken>,
        before_dispatch: impl FnOnce() + Send,
    ) -> Result<astra_mcp::McpToolCallResult, SourceInvocationError> {
        let invalid = |error: SourceError| {
            SourceInvocationError::Rpc(super::runtime_mcp::capability_source_rpc_error(
                error.to_string(),
            ))
        };
        // Preparation reads contracts and durable custody only. It may be
        // dropped on cancellation; an effectful RPC below must retain the
        // canonical transport's acknowledgement and settlement ownership.
        let prepare = async {
            self.request_scope(request).map_err(invalid)?;
            let materialized = self
                .materialize(&request.tool_name, false)
                .await
                .map_err(invalid)?;
            let scope = self
                .scope
                .get()
                .ok_or_else(|| invalid(SourceError::CustodyUnavailable))?;
            let receipt = self
                .custody
                .candidate_receipt(scope, &request.tool_name)
                .await
                .map_err(invalid)?;
            Ok::<_, SourceInvocationError>((materialized, receipt))
        };
        let (materialized, receipt) = match cancel_token {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(SourceInvocationError::CancelledBeforeDispatch),
                result = prepare => result?,
            },
            None => prepare.await?,
        };
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            return Err(SourceInvocationError::CancelledBeforeDispatch);
        }
        if request.policy.dispatch_deadline_rejection().is_some() {
            return Err(SourceInvocationError::AdmissionDeadlineExpired);
        }
        before_dispatch();
        let response = self
            .transport
            .capability_rpc(
                "astra/capabilities/invoke",
                json!({
                    "capability": materialized.candidate.capability, "arguments": request.args,
                    "discovery_tool": receipt.discovery_tool,
                    "call_id": request.tool_call_id, "interaction_response": interaction,
                }),
                true,
                cancel_token,
            )
            .await?;
        super::runtime_mcp::extract_capability_source_result(&response).map_err(Into::into)
    }

    pub(crate) fn owns_materialized(&self, key: &str) -> bool {
        self.materialized
            .read()
            .is_ok_and(|cache| cache.contains_key(key))
    }

    fn cached(&self, key: &str) -> Result<Option<MaterializedCapability>, SourceError> {
        Ok(self
            .materialized
            .read()
            .map_err(|_| SourceError::CustodyUnavailable)?
            .get(key)
            .cloned())
    }

    pub(crate) fn materialized_schemas(&self) -> Result<Vec<Value>, SourceError> {
        Ok(self
            .materialized
            .read()
            .map_err(|_| SourceError::CustodyUnavailable)?
            .values()
            .map(|entry| entry.schema.clone())
            .collect())
    }

    async fn load_skill(
        &self,
        key: &str,
    ) -> Result<astra_skills::traits::ResolvedSkill, SourceError> {
        let scope = self.scope.get().ok_or(SourceError::CustodyUnavailable)?;
        let _resolution = self.resolution.lock().await;
        let receipt = self.custody.candidate_receipt(scope, key).await?;
        let candidate = receipt.candidate;
        if candidate.kind != CapabilityKind::Skill {
            return Err(SourceError::InvalidContract);
        }
        let raw = match self.custody.descriptor(scope, key).await? {
            Some((_, raw)) => raw,
            None => {
                let response = self
                    .transport
                    .capability_rpc(
                        "astra/capabilities/read_skill",
                        json!({"capability": candidate.capability, "discovery_tool": receipt.discovery_tool}),
                        false,
                        self.cancel.get(),
                    )
                    .await
                    .map_err(|error| transport_failure(scope, "read_skill", &error))?;
                if response.get("capability")
                    != Some(
                        &serde_json::to_value(&candidate.capability).expect("reference serializes"),
                    )
                {
                    return Err(SourceError::SelectionChanged);
                }
                let encoded = response
                    .get("descriptor_json")
                    .and_then(Value::as_str)
                    .ok_or(SourceError::InvalidContract)?
                    .to_string();
                self.custody
                    .record_descriptor(scope, key, candidate, encoded)
                    .await?
            }
        };
        let contract: SourceSkillContract =
            serde_json::from_value(raw).map_err(|_| SourceError::InvalidContract)?;
        if contract.name.is_empty() {
            return Err(SourceError::InvalidContract);
        }
        Ok(astra_skills::traits::ResolvedSkill {
            name: key.to_string(),
            instructions: contract.instructions,
            allowed_tools: contract.allowed_tools,
            input_schema: contract.input_schema,
            output_schema: contract.output_schema,
            max_tokens: None,
            execution_context: astra_skills::manifest::ExecutionContext::Inline,
            hooks: Default::default(),
            skill_dir: None,
            source: astra_skills::manifest::SkillSourceKind::Plugin,
            success_criteria: Vec::new(),
            composition: None,
            remote_url: None,
            forward_headers: Vec::new(),
            required_headers: Vec::new(),
            aliases: Vec::new(),
            effort: None,
            agent_type: None,
            trust_tier: astra_skills::manifest::TrustTier::Verified,
        })
    }

    async fn read_skill_resource(
        &self,
        key: &str,
        path: &str,
    ) -> Result<astra_skills::traits::SkillResource, SourceError> {
        if !crate::turn::skill_tool::valid_skill_resource_path(path) {
            return Err(SourceError::InvalidContract);
        }
        let scope = self.scope.get().ok_or(SourceError::CustodyUnavailable)?;
        let receipt = self.custody.candidate_receipt(scope, key).await?;
        if receipt.candidate.kind != CapabilityKind::Skill
            || self.custody.descriptor(scope, key).await?.is_none()
        {
            return Err(SourceError::SelectionNotFound);
        }
        let response = self.transport.capability_rpc("astra/capabilities/read_skill", json!({
            "capability": receipt.candidate.capability, "discovery_tool": receipt.discovery_tool, "path": path
        }), false, self.cancel.get()).await.map_err(|error| transport_failure(scope, "read_skill_resource", &error))?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Resource {
            path: String,
            content: String,
            digest: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Response {
            capability: CapabilityReference,
            resource: Resource,
        }
        let response: Response =
            serde_json::from_value(response).map_err(|_| SourceError::InvalidContract)?;
        if response.capability != receipt.candidate.capability
            || response.resource.path != path
            || response.resource.content.len() > MAX_DESCRIPTOR_BYTES
            || response.resource.content.contains('\0')
            || response.resource.digest
                != format!(
                    "sha256:{:x}",
                    Sha256::digest(response.resource.content.as_bytes())
                )
        {
            return Err(SourceError::SelectionChanged);
        }
        Ok(astra_skills::traits::SkillResource {
            path: response.resource.path,
            content: response.resource.content,
        })
    }
}

pub(crate) struct CapabilitySourceSkillResolver {
    pub source: Arc<CapabilitySourceRuntime>,
    pub base: Option<Arc<dyn astra_skills::traits::SkillResolver>>,
    pub policy: crate::turn::skill_tool::SkillSurfacingPolicy,
}

#[async_trait::async_trait]
impl astra_skills::traits::SkillResolver for CapabilitySourceSkillResolver {
    fn resolve(
        &self,
        name: &str,
    ) -> Result<astra_skills::traits::ResolvedSkill, astra_skills::traits::SkillError> {
        if self.source.owns_selection_shape(name) {
            return Err(astra_skills::traits::SkillError::LoadFailed(
                "capability_source_requires_async_resolution".into(),
            ));
        }
        self.base
            .as_ref()
            .ok_or_else(|| astra_skills::traits::SkillError::NotFound(name.into()))?
            .resolve(name)
    }

    async fn resolve_for_execution(
        &self,
        name: &str,
    ) -> Result<astra_skills::traits::ResolvedSkill, astra_skills::traits::SkillError> {
        if !self.source.owns_selection_shape(name) {
            return self
                .base
                .as_ref()
                .ok_or_else(|| astra_skills::traits::SkillError::NotFound(name.into()))?
                .resolve_for_execution(name)
                .await;
        }
        if !self.policy.allows_dynamic_plugin_catalog() {
            return Err(astra_skills::traits::SkillError::PermissionDenied(
                "capability_source_skill_not_allowed".into(),
            ));
        }
        let skill = self
            .source
            .load_skill(name)
            .await
            .map_err(|error| astra_skills::traits::SkillError::LoadFailed(error.to_string()))?;
        if !self.policy.allows_resolved(&skill) {
            return Err(astra_skills::traits::SkillError::PermissionDenied(
                "capability_source_skill_not_allowed".into(),
            ));
        }
        Ok(skill)
    }

    async fn read_resource(
        &self,
        name: &str,
        path: &str,
    ) -> Result<astra_skills::traits::SkillResource, astra_skills::traits::SkillError> {
        self.resolve_for_execution(name).await?;
        if !self.source.owns_selection_shape(name) {
            return self
                .base
                .as_ref()
                .ok_or_else(|| astra_skills::traits::SkillError::NotFound(name.into()))?
                .read_resource(name, path)
                .await;
        }
        self.source
            .read_skill_resource(name, path)
            .await
            .map_err(|error| astra_skills::traits::SkillError::LoadFailed(error.to_string()))
    }

    fn available_skills(&self) -> Vec<astra_skills::traits::SkillToolInfo> {
        self.base
            .as_ref()
            .map(|resolver| resolver.available_skills())
            .unwrap_or_default()
    }

    fn has_dynamic_catalog(&self) -> bool {
        self.policy.allows_dynamic_plugin_catalog()
    }

    fn execution_catalog_contains(&self, name: &str) -> bool {
        self.base
            .as_ref()
            .is_some_and(|resolver| resolver.execution_catalog_contains(name))
    }

    fn execution_topology(
        &self,
        name: &str,
    ) -> Option<astra_skills::manifest::SkillExecutionTopology> {
        self.base
            .as_ref()
            .and_then(|resolver| resolver.execution_topology(name))
    }
}

fn transport_failure(
    scope: &SourceScope,
    operation: &str,
    error: &super::runtime_mcp::AgentBindingMcpRpcError,
) -> SourceError {
    tracing::warn!(component = "capability_source", operation, reason = "source_unavailable", user_id = scope.user_id,
        session_id = scope.session_id, run_id = scope.run_id, error = %error, "capability source transport failed");
    SourceError::CustodyUnavailable
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use astra_services::runs::InMemoryRunStateStore;

    fn config() -> CapabilitySourceConfig {
        CapabilitySourceConfig {
            contract_version: CONTRACT_VERSION.into(),
            selection_authorization: "discovery_scope".into(),
            discovery_tools: BTreeSet::from(["search".into()]),
        }
    }

    fn candidate() -> (CapabilityCandidate, String) {
        let descriptor = json!({"name":"get_me", "description":"Read the connected account", "side_effect_class":"read", "inputSchema":{"type":"object","additionalProperties":false}}).to_string();
        (
            CapabilityCandidate {
                kind: CapabilityKind::Tool,
                title: "Account".into(),
                summary: "Read account".into(),
                capability: CapabilityReference {
                    snapshot_revision: "inventory-v1".into(),
                    binding_id: "account-v1".into(),
                    native_id: "get_me".into(),
                    descriptor_digest: format!(
                        "sha256:{:x}",
                        Sha256::digest(descriptor.as_bytes())
                    ),
                    reference: "opaque-ref".into(),
                },
            },
            descriptor,
        )
    }

    async fn fixture() -> (Arc<InMemoryRunStateStore>, SourceScope) {
        let store = Arc::new(InMemoryRunStateStore::new());
        let engine = crate::server::run::engine::RunEngine::new(store.clone());
        engine
            .start_run("source-run", "source-user", "source-session")
            .await
            .unwrap();
        let run = engine
            .load_run("source-user", "source-run")
            .await
            .unwrap()
            .unwrap();
        (
            store,
            SourceScope {
                user_id: run.user_id,
                session_id: run.session_id,
                run_id: run.run_id,
                generation: run.run_generation,
            },
        )
    }

    async fn register(custody: &CapabilitySourceCustody, scope: &SourceScope) -> String {
        custody
            .register(
                scope,
                "search",
                CapabilityCandidatePage {
                    contract_version: CONTRACT_VERSION.into(),
                    candidates: vec![candidate().0],
                },
            )
            .await
            .unwrap()
            .remove(0)
            .selection_key
    }

    #[tokio::test]
    async fn source_selection_names_are_stable_but_discovery_is_run_scoped() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store.clone());
        let key = register(&custody, &scope).await;
        assert!(key.starts_with("mcp__cap__get_me__"), "{key}");
        assert_eq!(key.rsplit("__").next().unwrap().len(), 16);
        let mut other = scope.clone();
        other.run_id = "other-run".into();
        other.session_id = "other-session".into();
        let receipt = custody.receipt(&other, "search", candidate().0).unwrap();
        assert_eq!(custody.selection_key(&receipt), key);
        assert_eq!(
            custody.candidate(&other, &key).await.unwrap_err(),
            SourceError::SelectionNotFound
        );
        crate::server::run::engine::RunEngine::new(store.clone())
            .start_run(&other.run_id, &other.user_id, &other.session_id)
            .await
            .unwrap();
        assert_eq!(register(&custody, &other).await, key);
        assert_eq!(
            custody.candidate(&other, &key).await.unwrap(),
            candidate().0
        );

        for native_id in ["calendar", "日历/邮箱", &"长名字".repeat(20)] {
            let mut named = candidate().0;
            named.capability.native_id = native_id.into();
            let receipt = custody.receipt(&scope, "search", named).unwrap();
            let key = custody.selection_key(&receipt);
            assert!(key.len() <= 64);
            assert!(
                key.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            );
            assert!(custody.owns_selection_shape(&key));
            let mut renamed = receipt.clone();
            renamed.candidate.title = "Different display title".into();
            assert_eq!(custody.selection_key(&renamed), key);
            for changed in [
                "source",
                "config",
                "discovery",
                "binding",
                "reference",
                "snapshot",
                "digest",
                "kind",
                "native",
            ] {
                let mut distinct = receipt.clone();
                match changed {
                    "source" => distinct.source_id.push('2'),
                    "config" => distinct.config_digest.push('2'),
                    "discovery" => distinct.discovery_tool.push('2'),
                    "binding" => distinct.candidate.capability.binding_id.push('2'),
                    "reference" => distinct.candidate.capability.reference.push('2'),
                    "snapshot" => distinct.candidate.capability.snapshot_revision.push('2'),
                    "digest" => {
                        distinct.candidate.capability.descriptor_digest =
                            format!("sha256:{}", "a".repeat(64))
                    }
                    "kind" => distinct.candidate.kind = CapabilityKind::Skill,
                    "native" => distinct.candidate.capability.native_id.push('2'),
                    _ => unreachable!(),
                }
                assert_ne!(custody.selection_key(&distinct), key, "{changed}");
            }
        }
    }

    #[test]
    fn source_selection_label_preserves_whole_encoded_units() {
        assert_eq!(selection_label("Calendar:List"), "calendar_list");
        assert_eq!(selection_label("日历/邮箱"), "u65e5u5386_u90aeu7bb1");
        assert_eq!(
            selection_label(&format!("{}日", "a".repeat(22))),
            "a".repeat(22)
        );
        assert_eq!(
            selection_label(&"x".repeat(100)),
            "x".repeat(MAX_SELECTION_LABEL_BYTES)
        );
    }

    #[tokio::test]
    async fn source_selection_collision_does_not_overwrite_or_publish_siblings() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store.clone());
        let receipt = custody.receipt(&scope, "search", candidate().0).unwrap();
        let key = custody.selection_key(&receipt);
        // Force two distinct full receipts onto one short key without depending
        // on a probabilistic hash collision or adding a production hash override.
        let mut collided = receipt.clone();
        collided.candidate.capability.reference = "other-package-skill".into();
        store
            .append_event(
                &scope.user_id,
                &scope.session_id,
                &scope.run_id,
                receipt_event(CANDIDATE_EVENT, &key, &collided),
            )
            .await
            .unwrap();
        let mut sibling = candidate().0;
        sibling.capability.native_id = "sibling".into();
        let sibling_key =
            custody.selection_key(&custody.receipt(&scope, "search", sibling.clone()).unwrap());
        assert_eq!(
            custody
                .register(
                    &scope,
                    "search",
                    CapabilityCandidatePage {
                        contract_version: CONTRACT_VERSION.into(),
                        candidates: vec![sibling, candidate().0],
                    }
                )
                .await
                .unwrap_err(),
            SourceError::SelectionChanged
        );
        assert_eq!(
            custody.load(&scope, CANDIDATE_EVENT, &key).await.unwrap(),
            Some(serde_json::to_value(collided).unwrap())
        );
        assert_eq!(
            custody.candidate(&scope, &sibling_key).await.unwrap_err(),
            SourceError::SelectionNotFound
        );
    }

    #[tokio::test]
    async fn source_selection_namespace_never_falls_through_to_local_skills() {
        use astra_skills::traits::{SkillError, SkillResolver};
        let fixture = http_fixture(true).await;
        let source = fixture.source();
        source.bind_scope(fixture.scope.clone(), None).unwrap();
        let resolver = CapabilitySourceSkillResolver {
            source,
            base: None,
            policy: Default::default(),
        };
        for name in [
            "skill__cap__typo",
            "skill__cap__calendar__0123456789abcde",
            "skill__cap__calendar__0123456789abcdeg",
            "skill__cap__calendar__0123456789ABCDEF",
            "SKILL__CAP__calendar__0123456789abcdef",
            "skill__cap__aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                matches!(
                    resolver.resolve_for_execution(name).await,
                    Err(SkillError::LoadFailed(_))
                ),
                "{name}"
            );
            assert!(
                matches!(
                    resolver.read_resource(name, "references/calendar.md").await,
                    Err(SkillError::LoadFailed(_))
                ),
                "{name}"
            );
        }
        assert!(fixture.calls.lock().unwrap().is_empty());
    }

    pub(crate) struct HttpFixture {
        pub store: Arc<InMemoryRunStateStore>,
        pub scope: SourceScope,
        pub transport: Arc<super::super::runtime_mcp::AgentBindingMcpRuntime>,
        pub candidate: CapabilityCandidate,
        pub calls: Arc<std::sync::Mutex<Vec<Value>>>,
        pub bundle: super::super::runtime_mcp::RuntimeMcpBundle,
        endpoint_url: String,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl HttpFixture {
        pub(crate) fn source(&self) -> Arc<CapabilitySourceRuntime> {
            Arc::new(
                CapabilitySourceRuntime::new(self.transport.clone(), self.store.clone()).unwrap(),
            )
        }
    }

    pub(crate) async fn http_fixture(skill: bool) -> HttpFixture {
        http_fixture_with_inventory(skill, 1).await
    }

    pub(crate) async fn http_fixture_with_inventory(skill: bool, size: usize) -> HttpFixture {
        http_fixture_with_scope_inventory(skill, size, false, None).await
    }

    pub(crate) async fn http_fixture_with_bindings(skill: bool, size: usize) -> HttpFixture {
        http_fixture_with_scope_inventory(skill, size, true, None).await
    }

    pub(crate) async fn http_fixture_with_large_tool() -> HttpFixture {
        http_fixture_for_source(false, 1, false, None, "source", true).await
    }

    struct RpcGate {
        method: &'static str,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
        cancelled: tokio::sync::Notify,
    }

    impl RpcGate {
        fn new(method: &'static str) -> Self {
            Self {
                method,
                started: Default::default(),
                release: Default::default(),
                cancelled: Default::default(),
            }
        }
    }

    async fn http_fixture_with_scope_inventory(
        skill: bool,
        size: usize,
        distinct_bindings: bool,
        rpc_gate: Option<Arc<RpcGate>>,
    ) -> HttpFixture {
        http_fixture_for_source(skill, size, distinct_bindings, rpc_gate, "source", false).await
    }

    async fn http_fixture_for_source(
        skill: bool,
        size: usize,
        distinct_bindings: bool,
        rpc_gate: Option<Arc<RpcGate>>,
        source_id: &str,
        large_tool: bool,
    ) -> HttpFixture {
        assert!(size > 0);
        let (store, scope) = fixture().await;
        let (mut candidate, mut descriptor) = candidate();
        if large_tool {
            let mut exact: Value = serde_json::from_str(&descriptor).unwrap();
            exact["inputSchema"]["properties"] = Value::Object(
                (0..220)
                    .map(|index| (format!("optional_field_{index}"), json!({"type":"string"})))
                    .collect(),
            );
            descriptor = exact.to_string();
        }
        if skill {
            candidate.kind = CapabilityKind::Skill;
            candidate.capability.native_id = "review-repository".into();
            descriptor = json!({"name":"review-repository", "instructions":"Inspect the selected repository. Never send a message.", "allowed_tools":[]}).to_string();
            candidate.capability.descriptor_digest =
                format!("sha256:{:x}", Sha256::digest(descriptor.as_bytes()));
        }
        let inventory: Vec<_> = (0..size)
            .map(|index| {
                let mut item = candidate.clone();
                let mut exact: Value = serde_json::from_str(&descriptor).unwrap();
                if distinct_bindings {
                    item.capability.binding_id = format!("account-binding-{index}");
                    item.capability.snapshot_revision = format!("account-revision-{index}");
                }
                if index != 0 {
                    item.capability.native_id = format!("get_me_{index}");
                    item.capability.reference = format!("opaque-ref-{index}");
                    exact["name"] = json!(item.capability.native_id);
                }
                let exact = exact.to_string();
                item.capability.descriptor_digest =
                    format!("sha256:{:x}", Sha256::digest(exact.as_bytes()));
                (item, exact)
            })
            .collect();
        candidate = inventory[0].0.clone();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new().route("/mcp", axum::routing::post(
            |axum::extract::State((calls, inventory, gate)): axum::extract::State<(Arc<std::sync::Mutex<Vec<Value>>>, Vec<(CapabilityCandidate, String)>, Option<Arc<RpcGate>>)>,
             headers: axum::http::HeaderMap, axum::Json(request): axum::Json<Value>| async move {
                assert_eq!(headers.get("authorization").unwrap(), "Bearer test");
                calls.lock().unwrap().push(request.clone());
                if request["method"] == "notifications/cancelled" {
                    gate.as_ref().expect("cancellable invocation").cancelled.notify_one();
                    return axum::Json(json!({}));
                }
                if let Some(gate) = gate && request["method"] == gate.method {
                    gate.started.notify_one();
                    gate.release.notified().await;
                }
                let result = match request["method"].as_str().unwrap() {
                    "astra/capabilities/describe" | "astra/capabilities/read_skill" => {
						assert_eq!(request["params"]["discovery_tool"], "search");
                        let (candidate, descriptor) = inventory.iter().find(|(item, _)| request["params"]["capability"]["native_id"] == item.capability.native_id).unwrap();
                        assert_eq!(request["params"]["capability"], json!(candidate.capability));
						if let Some(path) = request["params"]["path"].as_str() {
							assert_eq!(candidate.kind, CapabilityKind::Skill);
							assert!(matches!(path, "references/guide.md" | "references/tampered.md"));
							let content = "A bounded reference. Never send a message.";
							let digest = if path == "references/tampered.md" { "sha256:tampered".to_string() } else { format!("sha256:{:x}", Sha256::digest(content.as_bytes())) };
							json!({"capability": candidate.capability, "resource": {"path": path, "content": content, "digest": digest}})
						} else {
                        json!({"capability": candidate.capability, "descriptor_json": descriptor})
						}
                    }
                    "astra/capabilities/invoke" => {
						assert_eq!(request["params"]["discovery_tool"], "search");
                        let (candidate, _) = inventory.iter().find(|(item, _)| request["params"]["capability"]["native_id"] == item.capability.native_id).unwrap();
                        assert_eq!(request["params"]["capability"], json!(candidate.capability));
                        assert!(request["params"]["call_id"].as_str().is_some_and(|id| !id.is_empty()));
                        json!({"content":[{"type":"text","text":"account-read-complete"}]})
                    }
                    "tools/call" => {
                        assert_eq!(request["params"]["name"], "search");
                        let query = request["params"]["arguments"]["q"].as_str();
                        let candidates: Vec<_> = inventory.iter().filter(|(item, _)| query.is_none_or(|query| item.capability.native_id == query)).take(MAX_PAGE).map(|(item, _)| item).collect();
                        json!({"content":[{"type":"text", "text":"ready"}],
                            "_meta": {CANDIDATES_METADATA_KEY: {"contract_version": CONTRACT_VERSION, "candidates":candidates}}})
                    },
                    unexpected => panic!("unexpected source RPC {unexpected}"),
                };
                axum::Json(json!({"jsonrpc":"2.0", "id":request["id"], "result":result}))
            }
        )).with_state((calls.clone(), inventory, rpc_gate));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let bundle = super::super::runtime_mcp::prepare_agent_binding_mcp_bundle_from_snapshot(
            source_id, &endpoint, "Bearer test", None, &[json!({"name":"search", "description":"Find capabilities", "side_effect_class":"read", "inputSchema":{"type":"object"}})]
        ).unwrap();
        let transport = Arc::new(
            bundle
                .agent_binding_mcp
                .as_ref()
                .unwrap()
                .as_ref()
                .clone()
                .with_capability_source(config())
                .unwrap(),
        );
        HttpFixture {
            store,
            scope,
            transport,
            candidate,
            calls,
            bundle,
            endpoint_url: endpoint,
            server,
        }
    }

    pub(crate) fn request(
        scope: &SourceScope,
        name: &str,
        args: Value,
    ) -> super::super::tool_execution_binding::ToolExecutionRequest {
        super::super::tool_execution_binding::ToolExecutionRequest {
            provider_stage_dispatch: None,
            user_id: scope.user_id.clone(),
            session_id: scope.session_id.clone(),
            run_id: scope.run_id.clone(),
            turn_chain_id: "source-turn".into(),
            tool_call_id: "source-call".into(),
            tool_name: name.into(),
            args,
            workspace: super::super::tool_execution_binding::WorkspaceBinding::none(),
            workspace_record: None,
            executor: super::super::tool_execution_binding::ExecutorBinding::request_scoped_mcp(),
            runtime: None,
            selected_offer: None,
            policy: Default::default(),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            runtime_edge_dispatch_authorization: None,
            runtime_edge_dispatch_authorization_required: false,
        }
    }

    pub(crate) async fn register_fixture_candidate(
        source: &CapabilitySourceRuntime,
        fixture: &HttpFixture,
    ) -> String {
        source.bind_scope(fixture.scope.clone(), None).unwrap();
        source
            .custody
            .register(
                &fixture.scope,
                "search",
                CapabilityCandidatePage {
                    contract_version: CONTRACT_VERSION.into(),
                    candidates: vec![fixture.candidate.clone()],
                },
            )
            .await
            .unwrap()
            .remove(0)
            .selection_key
    }

    #[tokio::test]
    async fn source_tool_search_cancellation_releases_resolution_and_endpoint_capacity() {
        use super::super::runtime_tool_executor::RuntimeToolExecutor;
        use super::super::tool_local_transport::ServerLocalToolTransport;
        let gate = Arc::new(RpcGate::new("astra/capabilities/describe"));
        let fixture = http_fixture_with_scope_inventory(false, 1, false, Some(gate.clone())).await;
        let source = fixture.source();
        let key = register_fixture_candidate(&source, &fixture).await;
        let mut held_permits = Vec::new();
        while let Ok(permit) =
            crate::capability_endpoint_pool::try_acquire_endpoint_permit(&fixture.endpoint_url)
        {
            held_permits.push(permit);
        }
        assert!(
            held_permits.pop().is_some(),
            "reserve exactly one slot for describe"
        );
        let workspace = tempfile::tempdir().unwrap();
        let mut executor = RuntimeToolExecutor::new(
            workspace.path().to_path_buf(),
            fixture.scope.user_id.clone(),
            fixture.scope.session_id.clone(),
            None,
            None,
        );
        executor.set_agent_binding_mcp(fixture.transport.clone());
        executor.set_capability_source(Some(source.clone()));
        let cancel = tokio_util::sync::CancellationToken::new();
        let search = request(
            &fixture.scope,
            "tool_search",
            json!({"query":format!("select:{key}")}),
        );
        let execution = executor.execute_server_local_tool(&search, Some(&cancel));
        tokio::pin!(execution);
        tokio::select! {
            _ = gate.started.notified() => {},
            result = &mut execution => panic!("describe must remain blocked: {:?}", result.result),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => panic!("describe did not start"),
        }
        assert!(source.resolution.try_lock().is_err());
        assert!(
            crate::capability_endpoint_pool::try_acquire_endpoint_permit(&fixture.endpoint_url)
                .is_err()
        );
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .unwrap()
            .result;
        assert!(result.is_error);
        let metadata = result.metadata.as_ref().unwrap();
        assert_eq!(
            metadata["error_kind"],
            astra_tools::TOOL_ERROR_KIND_CANCELLED
        );
        assert_eq!(metadata["status"], astra_tools::TOOL_ERROR_KIND_CANCELLED);
        assert_eq!(metadata["cancelled"], true);
        assert_eq!(metadata["retryable"], false);
        assert!(source.resolution.try_lock().is_ok());
        let permit =
            crate::capability_endpoint_pool::try_acquire_endpoint_permit(&fixture.endpoint_url)
                .expect("cancelled describe must release the exact occupied slot");
        drop(permit);
        drop(held_permits);
        gate.release.notify_one();
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["method"], "astra/capabilities/describe",
            "cancellation must never dispatch an effectful invoke"
        );
    }

    #[tokio::test]
    async fn source_invocation_rechecks_deadline_after_blocked_custody_resolution() {
        let fixture = http_fixture(false).await;
        let source = fixture.source();
        let key = register_fixture_candidate(&source, &fixture).await;
        source.materialize(&key, true).await.unwrap();
        tokio::time::pause();
        source.materialized.write().unwrap().clear();
        let resolution = source.resolution.lock().await;
        let mut invocation = request(&fixture.scope, &key, json!({}));
        invocation.policy.admission_deadline =
            Some((tokio::time::Instant::now() + std::time::Duration::from_secs(5)).into_std());
        assert!(invocation.policy.dispatch_deadline_rejection().is_none());
        let execution = source.invoke(&invocation, None, None, || {});
        tokio::pin!(execution);
        assert!(futures_util::poll!(&mut execution).is_pending());
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        drop(resolution);
        assert!(matches!(
            execution.await,
            Err(SourceInvocationError::AdmissionDeadlineExpired)
        ));
        assert_eq!(
            fixture.calls.lock().unwrap().len(),
            1,
            "after the initial describe, no invoke may dispatch across the cutoff"
        );
    }

    #[tokio::test]
    async fn source_invocation_cancels_blocked_custody_before_physical_dispatch() {
        let fixture = http_fixture(false).await;
        let source = fixture.source();
        let key = register_fixture_candidate(&source, &fixture).await;
        source.materialize(&key, true).await.unwrap();
        source.materialized.write().unwrap().clear();
        let resolution = source.resolution.lock().await;
        let invocation = request(&fixture.scope, &key, json!({}));
        let cancel = CancellationToken::new();
        let armed = std::sync::atomic::AtomicBool::new(false);
        let execution = source.invoke(&invocation, None, Some(&cancel), || {
            armed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        tokio::pin!(execution);
        assert!(futures_util::poll!(&mut execution).is_pending());
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .expect("preparation must settle without waiting for the blocked read");
        assert!(matches!(
            result,
            Err(SourceInvocationError::CancelledBeforeDispatch)
        ));
        assert!(!armed.load(std::sync::atomic::Ordering::SeqCst));
        drop(resolution);
        assert!(source.resolution.try_lock().is_ok());
        assert_eq!(
            fixture.calls.lock().unwrap().len(),
            1,
            "only the initial describe may dispatch"
        );
    }

    #[tokio::test]
    async fn source_invocation_cancellation_preserves_already_dispatched_acknowledgement() {
        let gate = Arc::new(RpcGate::new("astra/capabilities/invoke"));
        let fixture = http_fixture_with_scope_inventory(false, 1, false, Some(gate.clone())).await;
        let source = fixture.source();
        let key = register_fixture_candidate(&source, &fixture).await;
        source.materialize(&key, true).await.unwrap();
        let invocation = request(&fixture.scope, &key, json!({}));
        let cancel = CancellationToken::new();
        let execution = source.invoke(&invocation, None, Some(&cancel), || {});
        tokio::pin!(execution);
        tokio::select! {
            _ = gate.started.notified() => {},
            result = &mut execution => panic!("invoke must remain in flight: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => panic!("invoke did not dispatch"),
        }
        cancel.cancel();
        assert!(
            futures_util::poll!(&mut execution).is_pending(),
            "an in-flight invocation must not be dropped on cancellation"
        );
        tokio::select! {
            _ = gate.cancelled.notified() => {},
            result = &mut execution => panic!("notification must not settle original invocation: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => panic!("provider did not receive cancellation"),
        }
        assert!(futures_util::poll!(&mut execution).is_pending());
        gate.release.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), execution)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.output, "account-read-complete");
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(calls[1]["method"], "astra/capabilities/invoke");
        assert_eq!(calls[2]["method"], "notifications/cancelled");
        assert_eq!(calls[2]["params"]["requestId"], calls[1]["id"]);
        assert_ne!(calls[1]["id"], calls[0]["id"]);
    }

    #[tokio::test]
    async fn source_registered_discovery_alias_reaches_its_admitted_provider() {
        use super::super::runtime_tool_executor::RuntimeToolExecutor;
        use super::super::tool_local_transport::ServerLocalToolTransport;
        for source_id in ["cap", "CAP", "cap__nested"] {
            let fixture = http_fixture_for_source(false, 1, false, None, source_id, false).await;
            let source = fixture.source();
            source.bind_scope(fixture.scope.clone(), None).unwrap();
            let alias = fixture.transport.public_tool_name("search").unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let mut executor = RuntimeToolExecutor::new(
                workspace.path().into(),
                fixture.scope.user_id.clone(),
                fixture.scope.session_id.clone(),
                None,
                None,
            );
            executor.set_provider_policy_index(fixture.bundle.provider_policy_index.clone());
            executor.set_agent_binding_mcp(fixture.transport.clone());
            executor.set_capability_source(Some(source));
            assert!(
                executor.tool_runtime_ready(alias),
                "registered descriptor must be ready"
            );
            assert_eq!(
                fixture
                    .bundle
                    .provider_policy_index
                    .resolve(alias)
                    .unwrap()
                    .effect,
                astra_turn_types::ResolvedToolEffect::ReadOnly
            );
            let result = executor
                .execute_server_local_tool(&request(&fixture.scope, alias, json!({})), None)
                .await
                .result;
            assert!(!result.is_error, "{source_id}: {}", result.output);
            let discovered: Value = serde_json::from_str(&result.output).unwrap();
            assert!(
                discovered["capabilities"][0]["selection_key"]
                    .as_str()
                    .is_some_and(valid_selection_key)
            );
            let calls = fixture.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call["method"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["tools/call"]
            );
            assert_eq!(calls[0]["params"]["name"], "search");
        }
    }

    #[tokio::test]
    async fn scoped_skill_reads_notify_cancellation_and_wait_for_the_original_response() {
        use astra_skills::traits::SkillResolver;
        for resource in [false, true] {
            let gate = Arc::new(RpcGate::new("astra/capabilities/read_skill"));
            let fixture =
                http_fixture_with_scope_inventory(true, 1, false, Some(gate.clone())).await;
            let source = fixture.source();
            let key = register_fixture_candidate(&source, &fixture).await;
            let cancel = CancellationToken::new();
            source
                .bind_scope(fixture.scope.clone(), Some(&cancel))
                .unwrap();
            let resolver = CapabilitySourceSkillResolver {
                source,
                base: None,
                policy: Default::default(),
            };
            if resource {
                gate.release.notify_one();
                resolver.resolve_for_execution(&key).await.unwrap();
                gate.started.notified().await;
            }
            let read = async {
                if resource {
                    resolver
                        .read_resource(&key, "references/guide.md")
                        .await
                        .map(|_| ())
                } else {
                    resolver.resolve_for_execution(&key).await.map(|_| ())
                }
            };
            tokio::pin!(read);
            tokio::select! {
                _ = gate.started.notified() => {},
                result = &mut read => panic!("read must block at the provider: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => panic!("read did not dispatch"),
            }
            cancel.cancel();
            tokio::select! {
                _ = gate.cancelled.notified() => {},
                result = &mut read => panic!("cancel notification cannot settle the exchange: {result:?}"),
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => panic!("provider did not receive cancellation"),
            }
            assert!(futures_util::poll!(&mut read).is_pending());
            gate.release.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(1), read)
                .await
                .unwrap()
                .unwrap();
            let calls = fixture.calls.lock().unwrap();
            let original = calls
                .iter()
                .rev()
                .find(|call| call["method"] == "astra/capabilities/read_skill")
                .unwrap();
            let notification = calls
                .iter()
                .find(|call| call["method"] == "notifications/cancelled")
                .unwrap();
            assert_eq!(notification["params"]["requestId"], original["id"]);
            assert_eq!(original["params"].get("path").is_some(), resource);
        }
    }

    #[tokio::test]
    async fn source_invocation_cancel_settles_outer_workspace_owner() {
        use super::super::runtime_tool_executor::RuntimeToolExecutor;
        use super::super::tool_execution_binding::{
            ExecutorBinding, ToolTransportKind, WorkspaceBinding,
        };
        use super::super::tool_local_transport::{
            ServerLocalToolTransport, execute_local_transport,
        };

        let gate = Arc::new(RpcGate::new("astra/capabilities/invoke"));
        let fixture = http_fixture_with_scope_inventory(false, 1, false, Some(gate.clone())).await;
        let workspace = tempfile::tempdir().unwrap();
        let mut executor = RuntimeToolExecutor::new(
            workspace.path().into(),
            fixture.scope.user_id.clone(),
            fixture.scope.session_id.clone(),
            None,
            None,
        );
        executor.set_execution_bindings(
            WorkspaceBinding::server_sandbox(workspace.path()),
            ExecutorBinding::server_local(),
        );
        executor.set_provider_policy_index(fixture.bundle.provider_policy_index.clone());
        executor.set_agent_binding_mcp(fixture.transport.clone());
        let source = fixture.source();
        let key = register_fixture_candidate(&source, &fixture).await;
        executor.set_capability_source(Some(source));
        let selected = executor
            .execute_server_local_tool(
                &request(
                    &fixture.scope,
                    "tool_search",
                    json!({"query":format!("select:{key}")}),
                ),
                None,
            )
            .await
            .result;
        assert!(!selected.is_error, "{}", selected.output);
        let mut invocation = request(&fixture.scope, &key, json!({}));
        invocation.workspace = WorkspaceBinding::server_sandbox(workspace.path());
        let binding = astra_runtime_env::RunBinding::resolve(
            astra_runtime_env::WorkspaceBinding::server_sandbox(&fixture.scope.session_id),
            astra_runtime_env::ExecutorBinding::local_cli(),
            astra_runtime_env::RuntimeBinding::host_process("test-host"),
            astra_runtime_env::PolicyIntent::local_developer(),
            &astra_runtime_env::ToolRegistry::default(),
        );
        let cancel = CancellationToken::new();
        let executor = executor.into_shared();
        let execution = execute_local_transport(
            &invocation,
            &binding,
            (
                &invocation.workspace,
                &invocation.executor,
                ToolTransportKind::ServerLocal,
            ),
            executor.as_ref(),
            Some(cancel.clone()),
            false,
        );
        tokio::pin!(execution);
        tokio::select! {
            _ = gate.started.notified() => {},
            _ = &mut execution => panic!("invocation must remain owned"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => panic!("invoke did not start"),
        }
        cancel.cancel();
        tokio::select! {
            _ = gate.cancelled.notified() => {},
            result = &mut execution => panic!("provider cancellation must precede outer settlement: {:?}", result.result),
        }
        assert!(
            futures_util::poll!(&mut execution).is_pending(),
            "notification is not an acknowledgement"
        );
        gate.release.notify_one();
        let result = execution.await.result;
        assert!(!result.is_error, "{}", result.output);
        assert_eq!(result.output, "account-read-complete");
        assert_ne!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_observation_quarantined")),
            Some(&Value::Bool(true))
        );
        let owner = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            workspace.path(),
            None,
            std::time::Duration::from_secs(1),
        )
        .await
        .expect("next writer must be admitted after provider settlement");
        drop(owner);
    }

    // Predict the scripted model's selection without registering any authority.
    // The real discovery response must create the receipt during the loop.
    pub(crate) fn fixture_selection_key(
        source: &CapabilitySourceRuntime,
        fixture: &HttpFixture,
    ) -> String {
        let receipt = source
            .custody
            .receipt(&fixture.scope, "search", fixture.candidate.clone())
            .unwrap();
        source.custody.selection_key(&receipt)
    }

    #[tokio::test]
    async fn source_http_tool_describe_invoke_and_restart_use_exact_custody() {
        let fixture = http_fixture(false).await;
        let source =
            CapabilitySourceRuntime::new(fixture.transport.clone(), fixture.store.clone()).unwrap();
        let key = register_fixture_candidate(&source, &fixture).await;
        // Registration is metadata only, with no remote catalog/schema fanout.
        assert!(fixture.calls.lock().unwrap().is_empty());
        let materialized = source.materialize(&key, true).await.unwrap();
        assert_eq!(materialized.schema["function"]["name"], key);
        let _resolution = source.resolution.lock().await;
        assert!(
            source.owns_materialized(&key),
            "an in-flight unrelated resolution does not hide ready contracts"
        );
        drop(_resolution);
        source
            .invoke(&request(&fixture.scope, &key, json!({})), None, None, || {})
            .await
            .unwrap();
        let reopened =
            CapabilitySourceRuntime::new(fixture.transport.clone(), fixture.store.clone()).unwrap();
        reopened.bind_scope(fixture.scope.clone(), None).unwrap();
        reopened.materialize(&key, false).await.unwrap();
        reopened
            .invoke(&request(&fixture.scope, &key, json!({})), None, None, || {})
            .await
            .unwrap();
        assert_eq!(
            fixture
                .calls
                .lock()
                .unwrap()
                .iter()
                .map(|call| call["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "astra/capabilities/describe",
                "astra/capabilities/invoke",
                "astra/capabilities/invoke"
            ]
        );
        let mut wrong = request(&fixture.scope, &key, json!({}));
        wrong.session_id = "other-session".into();
        assert!(reopened.invoke(&wrong, None, None, || {}).await.is_err());
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn source_http_discovery_metadata_is_consumed_only_by_trusted_discovery_tool() {
        let fixture = http_fixture(false).await;
        let source =
            CapabilitySourceRuntime::new(fixture.transport.clone(), fixture.store.clone()).unwrap();
        source.bind_scope(fixture.scope.clone(), None).unwrap();
        let mut result = astra_mcp::McpToolCallResult {
            output: "ready".into(),
            is_error: false,
            structured_content: Some(json!({"ready":true})),
            protocol_metadata: Some(
                json!({CANDIDATES_METADATA_KEY:{"contract_version":CONTRACT_VERSION,"candidates":[fixture.candidate]}}),
            ),
        };
        source
            .register_result(
                &request(&fixture.scope, "mcp__source__search", json!({})),
                &mut result,
            )
            .await
            .unwrap();
        assert!(!result.output.contains("opaque-ref"));
        let selections: Value = serde_json::from_str(&result.output).unwrap();
        let key = selections["capabilities"][0]["selection_key"]
            .as_str()
            .unwrap();
        assert_eq!(
            selections["capabilities"][0]["activation"],
            format!("tool_search(select:{key})")
        );
        assert_eq!(
            source.custody.candidate(&fixture.scope, key).await.unwrap(),
            fixture.candidate
        );
        assert!(
            result
                .protocol_metadata
                .as_ref()
                .unwrap()
                .get(CANDIDATES_METADATA_KEY)
                .is_none()
        );
    }

    #[tokio::test]
    async fn source_skill_is_lazy_policy_scoped_and_reopens_without_a_remote_read() {
        use astra_skills::traits::SkillResolver;
        let fixture = http_fixture(true).await;
        let source = Arc::new(
            CapabilitySourceRuntime::new(fixture.transport.clone(), fixture.store.clone()).unwrap(),
        );
        let key = register_fixture_candidate(&source, &fixture).await;
        let resolver = CapabilitySourceSkillResolver {
            source,
            base: None,
            policy: Default::default(),
        };
        assert!(resolver.has_dynamic_catalog());
        assert!(
            resolver.available_skills().is_empty(),
            "the whole remote skill catalog is never enumerated"
        );
        assert!(fixture.calls.lock().unwrap().is_empty());
        let resolved = resolver.resolve_for_execution(&key).await.unwrap();
        assert_eq!(resolved.name, key);
        assert!(
            resolved
                .instructions
                .contains("Inspect the selected repository")
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        let reopened = Arc::new(
            CapabilitySourceRuntime::new(fixture.transport.clone(), fixture.store.clone()).unwrap(),
        );
        reopened.bind_scope(fixture.scope.clone(), None).unwrap();
        let reopened = CapabilitySourceSkillResolver {
            source: reopened,
            base: None,
            policy: Default::default(),
        };
        assert_eq!(
            reopened
                .resolve_for_execution(&key)
                .await
                .unwrap()
                .instructions,
            resolved.instructions
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
        let resource = reopened
            .read_resource(&key, "references/guide.md")
            .await
            .unwrap();
        assert_eq!(resource.path, "references/guide.md");
        assert_eq!(
            resource.content,
            "A bounded reference. Never send a message."
        );
        assert_eq!(fixture.calls.lock().unwrap().len(), 2);
        assert!(reopened.read_resource(&key, "../secret").await.is_err());
        assert_eq!(fixture.calls.lock().unwrap().len(), 2);
        let context = crate::turn::skill_tool::SkillContext {
            current_task: Some("Inspect the selected repository".into()),
            ..Default::default()
        };
        let activation = crate::turn::skill_tool::execute_skill_inline(
            &reopened,
            "skill",
            &json!({"skill_name": key}),
            &context,
        )
        .await;
        assert!(activation.success, "{}", activation.output);
        assert!(activation.activation.is_some());
        let resource = crate::turn::skill_tool::execute_skill_inline(
            &reopened,
            "skill",
            &json!({"skill_name": key, "resource_path": "references/guide.md"}),
            &context,
        )
        .await;
        assert!(resource.success, "{}", resource.output);
        assert!(resource.activation.is_none());
        let resource: Value = serde_json::from_str(&resource.output).unwrap();
        assert_eq!(resource["resource_path"], "references/guide.md");
        let denied = CapabilitySourceSkillResolver {
            source: reopened.source,
            base: None,
            policy: crate::turn::skill_tool::SkillSurfacingPolicy {
                allowed_names: Some(Default::default()),
                ..Default::default()
            },
        };
        assert!(!denied.has_dynamic_catalog());
        assert!(matches!(
            denied.resolve_for_execution(&key).await,
            Err(astra_skills::traits::SkillError::PermissionDenied(_))
        ));
        assert!(matches!(
            denied.read_resource(&key, "references/guide.md").await,
            Err(astra_skills::traits::SkillError::PermissionDenied(_))
        ));
        assert_eq!(fixture.calls.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn source_custody_reopens_exact_run_and_source_receipts() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store.clone());
        let key = register(&custody, &scope).await;
        assert_eq!(register(&custody, &scope).await, key);
        let (candidate, encoded) = candidate();
        custody
            .record_descriptor(&scope, &key, candidate.clone(), encoded.clone())
            .await
            .unwrap();
        let reopened = CapabilitySourceCustody::new("source", config(), store.clone());
        let (restored_candidate, raw) = reopened.descriptor(&scope, &key).await.unwrap().unwrap();
        assert_eq!(restored_candidate, candidate);
        let (snapshot, schema) = super::super::runtime_mcp::resolve_capability_source_tool(
            "source", &key, &candidate, raw,
        )
        .unwrap();
        assert_eq!(schema["function"]["name"], key);
        let policy =
            astra_turn_core::provider_resolution::ResolvedProviderPolicyIndex::from_snapshots(
                std::slice::from_ref(&snapshot),
            )
            .unwrap();
        let resolved = policy.resolve(&key).unwrap();
        assert_eq!(
            resolved.descriptor.identity.native_tool_id.as_str(),
            "get_me"
        );
        assert_eq!(
            resolved.descriptor.identity.provider_binding.as_str(),
            "source:account-v1"
        );
        assert!(resolved.is_read_only());
        let mut extended = policy.clone();
        extended.extend_snapshot(&snapshot).unwrap();
        assert_eq!(extended.len(), 1);
        let mut changed_scope = scope.clone();
        changed_scope.session_id = "other-session".into();
        assert_eq!(
            reopened.candidate(&changed_scope, &key).await.unwrap_err(),
            SourceError::SelectionChanged
        );
        changed_scope.run_id = "other-run".into();
        assert_eq!(
            reopened.candidate(&changed_scope, &key).await.unwrap_err(),
            SourceError::SelectionNotFound
        );
        let changed_config = CapabilitySourceCustody::new(
            "source",
            CapabilitySourceConfig {
                discovery_tools: BTreeSet::from(["different".into()]),
                ..config()
            },
            store,
        );
        assert_eq!(
            changed_config.candidate(&scope, &key).await.unwrap_err(),
            SourceError::SelectionChanged
        );
    }

    #[tokio::test]
    async fn source_custody_rejects_forged_or_unregistered_keys() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store.clone());
        let receipt = custody.receipt(&scope, "search", candidate().0).unwrap();
        let key = custody.selection_key(&receipt);
        assert_eq!(
            custody.candidate(&scope, &key).await.unwrap_err(),
            SourceError::SelectionNotFound
        );
        let mut forged = receipt.clone();
        forged.run_id = "another-run".into();
        store
            .append_event(
                &scope.user_id,
                &scope.session_id,
                &scope.run_id,
                receipt_event(CANDIDATE_EVENT, &key, &forged),
            )
            .await
            .unwrap();
        assert_eq!(
            custody.candidate(&scope, &key).await.unwrap_err(),
            SourceError::SelectionChanged
        );
    }

    #[tokio::test]
    async fn source_custody_validates_whole_page_before_writing() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store.clone());
        let mut bad = candidate().0;
        bad.capability.descriptor_digest = "not-a-digest".into();
        let page = CapabilityCandidatePage {
            contract_version: CONTRACT_VERSION.into(),
            candidates: vec![candidate().0, bad],
        };
        assert_eq!(
            custody.register(&scope, "search", page).await.unwrap_err(),
            SourceError::InvalidContract
        );
        let receipt = custody.receipt(&scope, "search", candidate().0).unwrap();
        assert_eq!(
            custody
                .candidate(&scope, &custody.selection_key(&receipt))
                .await
                .unwrap_err(),
            SourceError::SelectionNotFound
        );
        for native in ["search-other", "provider-text", ""] {
            assert_eq!(
                custody
                    .register(
                        &scope,
                        native,
                        CapabilityCandidatePage {
                            contract_version: CONTRACT_VERSION.into(),
                            candidates: vec![candidate().0]
                        }
                    )
                    .await
                    .unwrap_err(),
                SourceError::InvalidContract
            );
        }
        let page = CapabilityCandidatePage {
            contract_version: CONTRACT_VERSION.into(),
            candidates: vec![candidate().0; 9],
        };
        assert_eq!(
            custody.register(&scope, "search", page).await.unwrap_err(),
            SourceError::InvalidContract
        );
    }

    #[tokio::test]
    async fn source_custody_never_overwrites_an_existing_candidate() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store);
        let key = register(&custody, &scope).await;
        let mut changed = candidate().0;
        changed.summary = "changed summary".into();
        assert_eq!(
            custody
                .register(
                    &scope,
                    "search",
                    CapabilityCandidatePage {
                        contract_version: CONTRACT_VERSION.into(),
                        candidates: vec![changed]
                    }
                )
                .await
                .unwrap_err(),
            SourceError::SelectionChanged
        );
        assert_eq!(
            custody.candidate(&scope, &key).await.unwrap(),
            candidate().0
        );
    }

    #[tokio::test]
    async fn source_custody_generation_fences_new_registration_and_descriptor() {
        let (store, scope) = fixture().await;
        let custody = CapabilitySourceCustody::new("source", config(), store);
        let key = register(&custody, &scope).await;
        let mut stale = scope.clone();
        stale.generation += 1;
        assert_eq!(
            custody
                .register(
                    &stale,
                    "search",
                    CapabilityCandidatePage {
                        contract_version: CONTRACT_VERSION.into(),
                        candidates: vec![candidate().0]
                    }
                )
                .await
                .unwrap_err(),
            SourceError::RunSuperseded
        );
        let (candidate, raw) = candidate();
        assert_eq!(
            custody
                .record_descriptor(&stale, &key, candidate, raw)
                .await
                .unwrap_err(),
            SourceError::RunSuperseded
        );
        assert!(custody.descriptor(&scope, &key).await.unwrap().is_none());
    }

    #[test]
    fn source_descriptor_digest_name_and_schema_are_exact() {
        let (candidate, encoded) = candidate();
        assert!(validate_descriptor(&candidate, &encoded).is_ok());
        let changed = encoded.replace("get_me", "delete_repository");
        assert_eq!(
            validate_descriptor(&candidate, &changed).unwrap_err(),
            SourceError::InvalidContract
        );
        let mut wrong_identity = candidate.clone();
        wrong_identity.capability.native_id = "delete_repository".into();
        assert_eq!(
            validate_descriptor(&wrong_identity, &encoded).unwrap_err(),
            SourceError::InvalidContract
        );
        let mut skill = candidate;
        skill.kind = CapabilityKind::Skill;
        assert_eq!(
            validate_descriptor(&skill, &encoded).unwrap_err(),
            SourceError::InvalidContract
        );
    }

    #[test]
    fn source_descriptor_rejects_duplicate_keys_at_every_depth() {
        for encoded in [
            r#"{"name":"get_me","name":"get_me","inputSchema":{"type":"object"}}"#,
            r#"{"name":"get_me","inputSchema":{"type":"object","properties":{"q":{"type":"string","type":"number"}}}}"#,
        ] {
            let (mut candidate, _) = candidate();
            candidate.capability.descriptor_digest =
                format!("sha256:{:x}", Sha256::digest(encoded.as_bytes()));
            assert_eq!(
                validate_descriptor(&candidate, encoded).unwrap_err(),
                SourceError::InvalidContract
            );
        }
    }

    #[test]
    fn source_configuration_requires_an_explicit_versioned_contract() {
        assert!(
            CapabilitySourceConfig::parse(&Default::default())
                .unwrap()
                .is_none()
        );
        for raw in [
            json!({"contract_version":"v0","discovery_tools":["search"]}),
            json!({"contract_version":CONTRACT_VERSION,"discovery_tools":[]}),
            json!({"contract_version":CONTRACT_VERSION,"discovery_tools":["search"],"trust_me":true}),
        ] {
            assert_eq!(
                CapabilitySourceConfig::parse(&serde_json::Map::from_iter([(
                    CONFIG_METADATA_KEY.into(),
                    raw
                )]))
                .unwrap_err(),
                SourceError::InvalidContract
            );
        }
    }
}
