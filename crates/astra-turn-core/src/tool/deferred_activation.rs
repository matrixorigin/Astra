//! Helpers for the deferred-tool activation contract.
//!
//! Deferred tool entries are discovery metadata. A tool becomes executable
//! only when it is already visible in `tools[]` or its full schema-addressed
//! contract was selected with `tool_search(query="select:NAME")` and carried
//! through the stable `invoke_tool` protocol.

use std::collections::HashSet;

use serde_json::Value;

pub use astra_turn_types::DeferredToolActivation;

/// The stable native function exposed for invoking a tool selected from the
/// deferred catalog. The carrier itself is not a capability; its target is
/// re-admitted through the ordinary tool path.
pub const DEFERRED_TOOL_INVOCATION_CARRIER: &str = "invoke_tool";

/// Provenance for a lifecycle call authorized by the runtime control plane.
/// Runtime ownership is execution-round evidence: it allows a host-created
/// call or an exact provider carrier at a mandatory state boundary to cross
/// the common tool pipeline without pretending that the model selected a
/// deferred schema. The operation is deliberately typed so a host cannot
/// grant one lifecycle operation for another tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeControlInvocationKind {
    WorkEstablishment,
    WorkScheduler,
    WorkSettlement,
}

impl RuntimeControlInvocationKind {
    #[must_use]
    pub const fn tool_name(self) -> &'static str {
        match self {
            Self::WorkEstablishment => "start_work",
            Self::WorkScheduler => "run_next_work_item",
            Self::WorkSettlement => "settle_work_item",
        }
    }
}

/// The one stable native schema which transports a selected deferred target.
///
/// Keeping this small protocol resident lets the provider's declared tool list
/// remain byte-for-byte stable after discovery. The selected tool's full
/// schema never returns to `tools[]`; it is validated from the recorded
/// selection contract before normal admission and execution.
#[must_use]
pub fn deferred_tool_invocation_carrier_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": DEFERRED_TOOL_INVOCATION_CARRIER,
            "description": "Invoke one tool whose full contract was selected with tool_search, or the exact lifecycle transition required by the runtime. Use this for every deferred tool and for advanced fields absent from a resident tool's current schema.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "arguments"],
                "properties": {
                    "name": {"type": "string", "description": "Selected or runtime-required tool name."},
                    "arguments": {"type": "object", "description": "Arguments for that selected tool."}
                }
            }
        }
    })
}

/// Strict, provider-neutral contents of one carrier call.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredToolInvocation {
    pub name: String,
    pub arguments: Value,
}

/// One executable invocation with both provider and execution identities.
///
/// `physical_provider_call` is the exact canonical provider function call and
/// stays attached to transcript/call-id/cache identity. `logical_target_call`
/// is what all existing policy, Work-role, route, and execution machinery
/// must inspect. For ordinary calls the two values are identical. Neither
/// identity may be inferred back from the other once a carrier is involved.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalToolInvocation {
    physical_provider_call: Value,
    target: InvocationTarget,
}

/// One provider response after canonicalization, retaining provider order.
///
/// The two projections are intentionally exposed only as parallel views of
/// the same ordered records. Callers must never concatenate admitted and
/// rejected subsets to rebuild history: provider ordering is transcript and
/// prompt-cache evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalToolInvocationBatch {
    invocations: Vec<CanonicalToolInvocation>,
}

impl CanonicalToolInvocationBatch {
    #[must_use]
    pub fn new(invocations: Vec<CanonicalToolInvocation>) -> Self {
        Self { invocations }
    }

    #[must_use]
    pub fn invocations(&self) -> &[CanonicalToolInvocation] {
        &self.invocations
    }

    #[must_use]
    pub fn physical_provider_calls(&self) -> Vec<Value> {
        self.invocations
            .iter()
            .map(|call| call.physical_provider_call().clone())
            .collect()
    }

    #[must_use]
    pub fn logical_target_calls(&self) -> Vec<Value> {
        self.invocations
            .iter()
            .map(|call| call.logical_target_call().clone())
            .collect()
    }

    /// The same provider identity must address both views, exactly once and
    /// in the exact provider sequence. This makes joining execution outcomes
    /// back to transcript evidence deterministic.
    pub fn validate(&self) -> Result<(), &'static str> {
        let mut ids = HashSet::with_capacity(self.invocations.len());
        for invocation in &self.invocations {
            let physical_id = invocation
                .provider_call_id()
                .ok_or("provider call id is missing")?;
            let logical_id = invocation
                .logical_target_call()
                .get("id")
                .and_then(Value::as_str)
                .ok_or("logical target id is missing")?;
            if physical_id != logical_id {
                return Err("physical and logical call ids differ");
            }
            if !ids.insert(physical_id) {
                return Err("provider batch contains duplicate call ids");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
enum InvocationTarget {
    Direct,
    Deferred {
        logical_target_call: Value,
        activation: DeferredToolActivation,
    },
    RuntimeControl {
        logical_target_call: Value,
        kind: RuntimeControlInvocationKind,
    },
}

impl CanonicalToolInvocation {
    /// Preserve the single identity of a normal provider-native call.
    #[must_use]
    pub fn ordinary(canonical_call: Value) -> Self {
        Self {
            physical_provider_call: canonical_call,
            target: InvocationTarget::Direct,
        }
    }

    /// Construct a host-owned lifecycle invocation after the corresponding
    /// control-plane decision has been admitted.  This is intentionally not a
    /// deferred activation: no provider selection or schema-search evidence
    /// exists for a call synthesized by the runtime.
    pub fn runtime_control(
        canonical_call: Value,
        kind: RuntimeControlInvocationKind,
    ) -> Result<Self, &'static str> {
        let actual_name = crate::tool::args::shape::tool_call_name(&canonical_call)
            .ok_or("runtime control call is missing a tool name")?;
        if actual_name != kind.tool_name() {
            return Err("runtime control operation does not match its tool name");
        }
        Ok(Self {
            physical_provider_call: canonical_call.clone(),
            target: InvocationTarget::RuntimeControl {
                logical_target_call: canonical_call,
                kind,
            },
        })
    }

    /// Resolve the stable physical carrier into one runtime-authorized
    /// lifecycle transition. This is intentionally separate from deferred
    /// activation: the caller must already own the typed state boundary that
    /// requires `kind`, and the carrier target must match it exactly.
    pub fn runtime_control_from_carrier(
        canonical_call: &Value,
        kind: RuntimeControlInvocationKind,
    ) -> Result<Option<Self>, &'static str> {
        if crate::tool::args::shape::tool_call_name(canonical_call)
            != Some(DEFERRED_TOOL_INVOCATION_CARRIER)
        {
            return Ok(None);
        }
        let args = crate::tool::args::shape::parse_tool_call_arguments(canonical_call)
            .map_err(|_| "runtime control carrier arguments are malformed")?;
        let invocation = parse_deferred_tool_invocation(&args)?;
        if invocation.name != kind.tool_name() {
            return Ok(None);
        }
        let id = canonical_call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("runtime control carrier is missing its provider call id")?;
        let arguments = serde_json::to_string(&invocation.arguments)
            .map_err(|_| "runtime control arguments could not be serialized")?;
        let logical_target_call = serde_json::json!({
            "id": id,
            "type": "function",
            "function": {
                "name": invocation.name,
                "arguments": arguments,
            },
        });
        Ok(Some(Self {
            physical_provider_call: canonical_call.clone(),
            target: InvocationTarget::RuntimeControl {
                logical_target_call,
                kind,
            },
        }))
    }

    #[must_use]
    pub fn provider_call_id(&self) -> Option<&str> {
        self.physical_provider_call
            .get("id")
            .and_then(Value::as_str)
    }

    #[must_use]
    pub fn physical_provider_call(&self) -> &Value {
        &self.physical_provider_call
    }

    #[must_use]
    pub fn logical_target_call(&self) -> &Value {
        match &self.target {
            InvocationTarget::Direct => &self.physical_provider_call,
            InvocationTarget::Deferred {
                logical_target_call,
                ..
            } => logical_target_call,
            InvocationTarget::RuntimeControl {
                logical_target_call,
                ..
            } => logical_target_call,
        }
    }

    #[must_use]
    pub fn activation(&self) -> Option<&DeferredToolActivation> {
        match &self.target {
            InvocationTarget::Direct => None,
            InvocationTarget::Deferred { activation, .. } => Some(activation),
            InvocationTarget::RuntimeControl { .. } => None,
        }
    }

    #[must_use]
    pub fn runtime_control_kind(&self) -> Option<RuntimeControlInvocationKind> {
        match &self.target {
            InvocationTarget::RuntimeControl { kind, .. } => Some(*kind),
            InvocationTarget::Direct | InvocationTarget::Deferred { .. } => None,
        }
    }
}

/// Admission, policy, and execution consume the logical target by default.
/// Provider history deliberately uses [`CanonicalToolInvocation::physical_provider_call`]
/// explicitly, so a carrier cannot accidentally leak into an execution path.
impl std::ops::Deref for CanonicalToolInvocation {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        self.logical_target_call()
    }
}

/// Compatibility name for a resolved carrier invocation. New runtime code
/// should use [`CanonicalToolInvocation`] so normal and deferred calls share
/// one admission representation.
pub type CanonicalDeferredToolInvocation = CanonicalToolInvocation;

/// Why a carrier request cannot be converted into its logical target. These
/// are protocol facts, not policy decisions: after a successful conversion,
/// normal admission still decides whether the target may execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredToolInvocationError {
    Malformed,
    NotActivated,
    ActivationStale,
}

impl DeferredToolInvocationError {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "invoke_tool arguments do not match the carrier contract",
            Self::NotActivated => {
                "invoke_tool target was not selected from this session's deferred catalog"
            }
            Self::ActivationStale => {
                "invoke_tool target schema changed or is no longer available; select it again"
            }
        }
    }
}

/// Decode carrier arguments without accepting a second schema or any hidden
/// routing field. The target name is normalized with the same canonical-name
/// rule as ordinary function calls; unknown/empty names and non-object target
/// arguments fail before policy admission.
pub fn parse_deferred_tool_invocation(
    args: &Value,
) -> Result<DeferredToolInvocation, &'static str> {
    let object = args
        .as_object()
        .ok_or("invoke_tool arguments must be a JSON object")?;
    if object.len() != 2 || !object.contains_key("name") || !object.contains_key("arguments") {
        return Err("invoke_tool accepts exactly name and arguments");
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .and_then(astra_core::canonical_names::normalize_name)
        .ok_or("invoke_tool target name is missing")?
        .to_string();
    let arguments = object
        .get("arguments")
        .filter(|arguments| arguments.is_object())
        .cloned()
        .ok_or("invoke_tool target arguments must be a JSON object")?;
    Ok(DeferredToolInvocation { name, arguments })
}

/// Resolve one already-canonical provider carrier call into its paired
/// physical-provider and logical-target identities.
///
/// The caller supplies the session's retained activation evidence and the
/// *current* schema digest lookup. This keeps the transformation independent
/// of any particular executor while making stale provider/schema bindings fail
/// before ordinary admission and dispatch. A non-carrier call is returned as
/// `Ok(None)` so the shared path remains unchanged.
pub fn canonicalize_deferred_tool_invocation<F>(
    canonical_call: &Value,
    activations: &[DeferredToolActivation],
    current_schema_digest: F,
) -> Result<Option<CanonicalDeferredToolInvocation>, DeferredToolInvocationError>
where
    F: Fn(&str) -> Option<String>,
{
    let name = crate::tool::args::shape::tool_call_name(canonical_call)
        .ok_or(DeferredToolInvocationError::Malformed)?;
    if name != DEFERRED_TOOL_INVOCATION_CARRIER {
        return Ok(None);
    }
    let args = crate::tool::args::shape::parse_tool_call_arguments(canonical_call)
        .map_err(|_| DeferredToolInvocationError::Malformed)?;
    let invocation = parse_deferred_tool_invocation(&args)
        .map_err(|_| DeferredToolInvocationError::Malformed)?;
    // The carrier is transport, never a deferred capability. Permitting it as
    // a target would create a recursive and ambiguous execution path.
    if invocation.name == DEFERRED_TOOL_INVOCATION_CARRIER {
        return Err(DeferredToolInvocationError::NotActivated);
    }
    let mut matching_activations = activations
        .iter()
        .filter(|activation| activation.name == invocation.name);
    let activation = matching_activations
        .next()
        .ok_or(DeferredToolInvocationError::NotActivated)?;
    // A session projection with two digest revisions for one logical target
    // is ambiguous. Never let iteration order decide execution authority.
    if matching_activations.next().is_some() {
        return Err(DeferredToolInvocationError::ActivationStale);
    }
    if current_schema_digest(&invocation.name).as_deref() != Some(activation.schema_digest.as_str())
    {
        return Err(DeferredToolInvocationError::ActivationStale);
    }
    let id = canonical_call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or(DeferredToolInvocationError::Malformed)?;
    let arguments = serde_json::to_string(&invocation.arguments)
        .map_err(|_| DeferredToolInvocationError::Malformed)?;
    let logical_target_call = serde_json::json!({
        "id": id,
        "type": "function",
        "function": {
            "name": invocation.name,
            "arguments": arguments,
        },
    });
    Ok(Some(CanonicalDeferredToolInvocation {
        physical_provider_call: canonical_call.clone(),
        target: InvocationTarget::Deferred {
            logical_target_call,
            activation: activation.clone(),
        },
    }))
}

/// Canonicalize one provider batch into paired invocations.
///
/// Ordinary calls retain their single canonical value. A carrier is resolved
/// only from schema-addressed activation evidence supplied by retained
/// canonical history; callers receive a per-call result so one stale carrier
/// cannot erase independent calls in a mixed provider batch.
pub fn canonicalize_tool_invocation_batch<F>(
    provider_calls: &[Value],
    activations: &[DeferredToolActivation],
    current_schema_digest: F,
) -> Vec<Result<CanonicalToolInvocation, DeferredToolInvocationError>>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    provider_calls
        .iter()
        .map(|provider_call| {
            let canonical =
                crate::tool::args::shape::canonicalize_tool_call_for_execution(provider_call)
                    .map_err(|_| DeferredToolInvocationError::Malformed)?;
            match canonicalize_deferred_tool_invocation(
                &canonical,
                activations,
                current_schema_digest,
            )? {
                Some(invocation) => Ok(invocation),
                None => Ok(CanonicalToolInvocation::ordinary(canonical)),
            }
        })
        .collect()
}

/// Current tool surface installed by the runtime for admission/search.
///
/// `Uninstalled` means no LLM-request surface has been installed yet; callers
/// must fail closed because they cannot prove the model saw any tool schema.
/// `Installed { visible: ∅, activatable: ∅ }` is different: the runtime
/// deliberately sent a no-tool turn. That turn must not discard previously
/// activated deferred tools, because no schema-injection opportunity occurred.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolSurfaceNames {
    #[default]
    Uninstalled,
    Installed {
        visible: HashSet<String>,
        activatable: HashSet<String>,
    },
}

impl ToolSurfaceNames {
    #[must_use]
    pub fn installed(visible: HashSet<String>, activatable: HashSet<String>) -> Self {
        Self::Installed {
            visible,
            activatable,
        }
    }

    #[must_use]
    pub fn visible(&self) -> Option<&HashSet<String>> {
        match self {
            Self::Uninstalled => None,
            Self::Installed { visible, .. } => Some(visible),
        }
    }

    #[must_use]
    pub fn activatable(&self) -> Option<&HashSet<String>> {
        match self {
            Self::Uninstalled => None,
            Self::Installed { activatable, .. } => Some(activatable),
        }
    }

    #[must_use]
    pub fn visible_contains(&self, name: &str) -> bool {
        self.visible().is_some_and(|visible| visible.contains(name))
    }

    #[must_use]
    pub fn activatable_contains(&self, name: &str) -> bool {
        self.activatable()
            .is_some_and(|activatable| activatable.contains(name))
    }
}

/// Build the per-turn tool-search pool from the tools that are already
/// visible plus the deferred tools advertised in this turn's prompt.
///
/// `None` means no caller-installed surface exists yet; callers should fail
/// closed instead of falling back to a global catalog.
#[must_use]
pub fn searchable_tool_names(surface: &ToolSurfaceNames) -> Option<HashSet<String>> {
    match surface {
        ToolSurfaceNames::Uninstalled => None,
        ToolSurfaceNames::Installed {
            visible,
            activatable,
        } => {
            let mut names = HashSet::new();
            names.extend(visible.iter().cloned());
            names.extend(activatable.iter().cloned());
            Some(names)
        }
    }
}

/// Keep only tool names that the current runtime can actually execute.
#[must_use]
pub fn runtime_bound_tool_names<F>(
    names: HashSet<String>,
    has_runtime_binding: F,
) -> HashSet<String>
where
    F: Fn(&str) -> bool,
{
    names
        .into_iter()
        .filter(|name| has_runtime_binding(name))
        .collect()
}

/// Build the tool-search pool and remove names that the current runtime
/// cannot execute. This keeps `tool_search` aligned with the real execution
/// surface instead of cached or declarative schemas.
#[must_use]
pub fn searchable_runtime_bound_tool_names<F>(
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Option<HashSet<String>>
where
    F: Fn(&str) -> bool,
{
    let names = searchable_tool_names(surface)?;
    Some(runtime_bound_tool_names(names, has_runtime_binding))
}

/// Extract typed, schema-addressed activation evidence from a
/// `tool_search(select:...)` result.
///
/// This fails closed unless every accepted match carries a canonical SHA-256
/// digest and the producer returns the complete selection envelope. It is the
/// carrier protocol's boundary: transcript prose, a bare tool name, or a
/// hand-written/partial result cannot authorize a deferred invocation.
#[must_use]
pub fn deferred_tool_activations_from_tool_search_output(
    output: &str,
) -> Vec<DeferredToolActivation> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    if value.get("mode").and_then(Value::as_str) != Some("select") {
        return Vec::new();
    }
    let Some(query) = value.get("query").and_then(Value::as_str) else {
        return Vec::new();
    };
    let requested = requested_tool_names_from_select_query(query);
    if requested.is_empty() {
        return Vec::new();
    }
    let Some(output_requested) = requested_tool_names_from_output(&value) else {
        return Vec::new();
    };
    if !requested_tool_names_match(&requested, &output_requested) {
        return Vec::new();
    }
    // `resolved` is part of the producer-owned selection contract. Do not
    // infer it from `requested`: a partial/old/hand-written payload must not
    // silently turn a bare match into execution authority.
    let Some(activation_candidates) = resolved_tool_names_from_output(&value) else {
        return Vec::new();
    };
    let Some(matches) = value.get("matches").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut activations = Vec::new();
    for entry in matches {
        let Some(name) = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .and_then(astra_core::canonical_names::normalize_name)
        else {
            continue;
        };
        if !activation_candidates
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let Some(schema_digest) = entry
            .get("schema_digest")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|digest| is_schema_digest(digest))
        else {
            continue;
        };
        if !activations
            .iter()
            .any(|existing: &DeferredToolActivation| existing.name.eq_ignore_ascii_case(name))
        {
            activations.push(DeferredToolActivation {
                name: name.to_string(),
                schema_digest: schema_digest.to_string(),
                descriptor: None,
            });
        }
    }
    activations
}

/// Keep only schema-addressed selections that this turn's deferred manifest
/// actually offered and that the current runtime can bind. Visible tools
/// (notably the carrier itself) are searchable for discovery but never become
/// carrier targets merely because a model selected them.
#[must_use]
pub fn recordable_deferred_tool_activations<F>(
    output: &str,
    surface: &ToolSurfaceNames,
    has_runtime_binding: F,
) -> Vec<DeferredToolActivation>
where
    F: Fn(&str) -> bool,
{
    let Some(activatable) = surface.activatable() else {
        return Vec::new();
    };
    deferred_tool_activations_from_tool_search_output(output)
        .into_iter()
        .filter(|activation| {
            activation.name != DEFERRED_TOOL_INVOCATION_CARRIER
                && activatable.contains(&activation.name)
                && has_runtime_binding(&activation.name)
        })
        .collect()
}

/// Apply newly selected evidence to a session's carrier activation state.
///
/// State is keyed by canonical logical tool name, never by `(name, digest)`:
/// a later explicit selection replaces the earlier schema revision. This makes
/// a provider/schema refresh deterministic instead of allowing two otherwise
/// valid records to make dispatch depend on insertion order.
pub fn refresh_deferred_tool_activations<I>(
    activations: &mut Vec<DeferredToolActivation>,
    updates: I,
) where
    I: IntoIterator<Item = DeferredToolActivation>,
{
    for update in updates {
        activations.retain(|existing| existing.name != update.name);
        activations.push(update);
    }
    activations.sort_by(|left, right| left.name.cmp(&right.name));
}

fn is_schema_digest(value: &str) -> bool {
    value.len() == "sha256:".len() + 64
        && value.starts_with("sha256:")
        && value["sha256:".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

/// Reconstruct schema-addressed carrier evidence from canonical paired tool
/// history. Results without a compact contract digest contribute nothing: a
/// resumed session can reselect, but never obtains execution authority from
/// lossy state.
#[must_use]
pub fn deferred_tool_activations_from_messages(messages: &[Value]) -> Vec<DeferredToolActivation> {
    let mut pending_tool_search_call_ids = HashSet::new();
    let mut activations = Vec::new();
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                pending_tool_search_call_ids.extend(
                    message
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|call| {
                            call.get("function")
                                .and_then(|function| function.get("name"))
                                .and_then(Value::as_str)
                                .is_some_and(|name| name.eq_ignore_ascii_case("tool_search"))
                        })
                        .filter_map(|call| call.get("id").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(ToOwned::to_owned),
                );
            }
            Some("tool") => {
                let paired = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .is_some_and(|id| pending_tool_search_call_ids.remove(id));
                if !paired {
                    continue;
                }
                let Some(content) = message.get("content") else {
                    continue;
                };
                let serialized;
                let output = if let Some(content) = content.as_str() {
                    content
                } else {
                    serialized = match serde_json::to_string(content) {
                        Ok(serialized) => serialized,
                        Err(_) => continue,
                    };
                    serialized.as_str()
                };
                refresh_deferred_tool_activations(
                    &mut activations,
                    deferred_tool_activations_from_tool_search_output(output),
                );
            }
            _ => {}
        }
    }
    activations
}

/// Merge durable schema-addressed activation evidence with evidence still
/// present in canonical history. An entry without a digest is never
/// synthesized here: a resumed carrier call must have the exact schema
/// revision that was selected.
#[must_use]
pub fn merged_deferred_tool_activations(
    messages: &[Value],
    persisted_activations: impl IntoIterator<Item = DeferredToolActivation>,
) -> Vec<DeferredToolActivation> {
    let mut activations: Vec<DeferredToolActivation> = Vec::new();
    refresh_deferred_tool_activations(&mut activations, persisted_activations);
    // Transcript history only carries the compact selection candidate.  It
    // must never replace a durable activation that has already been bound to
    // an exact provider descriptor: doing so would erase the identity during
    // the next turn or after compaction/resume and silently turn a valid
    // activation into a descriptor-less one.  History may still fill a name
    // absent from the persisted snapshot, which keeps replay deterministic
    // for older turns without allowing it to downgrade newer state.
    for candidate in deferred_tool_activations_from_messages(messages) {
        let already_bound = activations.iter().any(|existing| {
            existing.name.eq_ignore_ascii_case(&candidate.name) && existing.descriptor.is_some()
        });
        if !already_bound {
            refresh_deferred_tool_activations(&mut activations, [candidate]);
        }
    }
    activations
}

fn requested_tool_names_from_select_query(query: &str) -> Vec<String> {
    let query = query.trim_start();
    const SELECT_PREFIX: &str = "select:";
    if !query
        .get(..SELECT_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SELECT_PREFIX))
    {
        return Vec::new();
    }

    let mut names = Vec::new();
    for name in query[SELECT_PREFIX.len()..]
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    names
}

fn requested_tool_names_from_output(value: &Value) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for name in value.get("requested")?.as_array()? {
        let name = name.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    Some(names)
}

fn resolved_tool_names_from_output(value: &Value) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for name in value.get("resolved")?.as_array()? {
        let name = name.as_str()?.trim();
        if name.is_empty() {
            return None;
        }
        if !names
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            names.push(name.to_string());
        }
    }
    Some(names)
}

fn requested_tool_names_match(left: &[String], right: &[String]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

#[must_use]
pub fn tool_not_admitted_message(name: &str, deferred_select_allowed: bool) -> String {
    if deferred_select_allowed {
        format!(
            "Error: Tool '{name}' is not available in this turn yet. It appears \
             in `<deferred-tools>`, so first call `tool_search` with \
             `query=\"select:{name}\"` to activate its full schema for the \
             next model request. Call `{name}` only after it appears in \
             `tools[]`, using the schema's exact fields."
        )
    } else {
        format!(
            "Error: Tool '{name}' is not available in this turn. Call only tools \
             visible in this turn's `tools[]`. If you need a deferred tool, it \
             must appear in this turn's `<deferred-tools>` before you can select \
             it with `tool_search`. If the tool is hidden by interaction mode or \
             policy, use a visible tool or ask in the normal response."
        )
    }
}

#[must_use]
pub fn deferred_tool_not_activatable_message(name: &str) -> String {
    format!(
        "Error: Tool '{name}' is listed in this turn's `<deferred-tools>`, \
         but it is not activatable in the current runtime surface. Do not \
         retry `{name}` or `tool_search(query=\"select:{name}\")` in this \
         turn; use visible tools or explain that the deferred capability is \
         currently unavailable."
    )
}

/// Outcome of a direct call to a tool name that is not in the visible
/// `tools[]` surface but may be advertised in `<deferred-tools>`.
///
/// First-principle: a direct call to a deferred tool is an *activation intent*,
/// not an executable request. The model has not seen the tool's full schema, so
/// the supplied arguments cannot be trusted. When the name is advertised in
/// `<deferred-tools>` and the runtime can execute it, we record the activation
/// and ask the model to retry on the next model request once the schema is
/// visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectDeferredCallAdmission {
    /// The name is in the deferred manifest AND has a runtime binding. Treat
    /// the direct call as an activation intent: record the name so the next
    /// model request's `tools[]` includes the full schema, then ask the model
    /// to retry with the schema's exact fields. Do NOT execute the supplied
    /// args.
    Activate { name: String },
    /// The name is in the deferred manifest but has no runtime binding on this
    /// runtime. Cannot activate; the not-admitted hint lets the model
    /// self-correct (the tool genuinely cannot run here).
    NotAdmitted,
    /// The name is not in the deferred manifest at all. It is either
    /// hallucinated or hidden by policy — return the unknown-tool body.
    Unknown,
}

/// Classify a direct call to `name` given whether it is advertised in this
/// turn's `<deferred-tools>` and whether the current runtime can execute it.
#[must_use]
pub fn classify_direct_deferred_call<F>(
    name: &str,
    is_deferred: bool,
    has_runtime_binding: F,
) -> DirectDeferredCallAdmission
where
    F: Fn(&str) -> bool,
{
    if !is_deferred {
        return DirectDeferredCallAdmission::Unknown;
    }
    if has_runtime_binding(name) {
        DirectDeferredCallAdmission::Activate {
            name: name.to_string(),
        }
    } else {
        DirectDeferredCallAdmission::NotAdmitted
    }
}

/// Message returned when a direct deferred call is treated as an activation
/// intent. The message is deliberately non-immediate: the failed direct call
/// must not become a same-batch retry loop while the schema is still absent.
#[must_use]
pub fn direct_deferred_call_activation_message(name: &str) -> String {
    format!(
        "Tool '{name}' is deferred and is not currently present in `tools[]`. \
         If `tool_search` is available, call `tool_search(query=\"select:{name}\")` \
         once to request activation. The direct call was not executed and its \
         arguments were ignored. Do not call `{name}` again in the same \
         tool-call batch; only invoke it after a later model request shows \
         `{name}` in `tools[]`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn searchable_names_fail_closed_without_installed_surface() {
        assert!(
            searchable_tool_names(&ToolSurfaceNames::Uninstalled).is_none(),
            "missing per-turn surface must not fall back to a global catalog"
        );
    }

    #[test]
    fn searchable_names_are_visible_union_activatable() {
        let visible = HashSet::from(["bash".to_string(), "tool_search".to_string()]);
        let activatable = HashSet::from(["web_fetch".to_string()]);
        let surface = ToolSurfaceNames::installed(visible, activatable);

        let names = searchable_tool_names(&surface)
            .expect("installed surface should produce a search pool");

        assert_eq!(
            names,
            HashSet::from([
                "bash".to_string(),
                "tool_search".to_string(),
                "web_fetch".to_string()
            ])
        );
    }

    #[test]
    fn searchable_runtime_bound_names_filter_visible_and_activatable() {
        let visible = HashSet::from([
            "bash".to_string(),
            "mcp__stale_visible".to_string(),
            "tool_search".to_string(),
        ]);
        let activatable =
            HashSet::from(["web_fetch".to_string(), "mcp__stale_deferred".to_string()]);
        let surface = ToolSurfaceNames::installed(visible, activatable);

        let names =
            searchable_runtime_bound_tool_names(&surface, |name| !name.starts_with("mcp__stale"))
                .expect("installed surface should produce a search pool");

        assert_eq!(
            names,
            HashSet::from([
                "bash".to_string(),
                "tool_search".to_string(),
                "web_fetch".to_string()
            ])
        );
    }

    #[test]
    fn recordable_carrier_activations_require_deferred_manifest_not_visibility() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let output = json!({
            "mode": "select",
            "query": "select:invoke_tool,read_file,web_fetch",
            "requested": ["invoke_tool", "read_file", "web_fetch"],
            "resolved": ["invoke_tool", "read_file", "web_fetch"],
            "matches": [
                {"name": "invoke_tool", "schema_digest": digest},
                {"name": "read_file", "schema_digest": format!("sha256:{}", "b".repeat(64))},
                {"name": "web_fetch", "schema_digest": format!("sha256:{}", "c".repeat(64))}
            ],
            "missing": []
        })
        .to_string();
        let surface = ToolSurfaceNames::installed(
            HashSet::from([
                DEFERRED_TOOL_INVOCATION_CARRIER.to_string(),
                "read_file".to_string(),
            ]),
            HashSet::from(["web_fetch".to_string()]),
        );

        assert_eq!(
            recordable_deferred_tool_activations(&output, &surface, |_| true),
            vec![DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: format!("sha256:{}", "c".repeat(64)),
                descriptor: None,
            }]
        );
    }

    #[test]
    fn carrier_activation_requires_schema_addressed_selection_evidence() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let selected = json!({
            "mode": "select",
            "query": "select:github,web_fetch",
            "requested": ["github", "web_fetch"],
            "resolved": ["github", "web_fetch"],
            "matches": [
                {"name": "github", "schema_digest": digest},
                {"name": "web_fetch"}
            ],
            "missing": []
        })
        .to_string();

        assert_eq!(
            deferred_tool_activations_from_tool_search_output(&selected),
            vec![DeferredToolActivation {
                name: "github".to_string(),
                schema_digest: format!("sha256:{}", "a".repeat(64)),
                descriptor: None,
            }],
            "a carrier must never treat a bare name as authorization"
        );
    }

    #[test]
    fn carrier_activation_rejects_forged_or_malformed_selection_evidence() {
        let valid_digest = format!("sha256:{}", "b".repeat(64));
        let base = json!({
            "mode": "select",
            "query": "select:github",
            "requested": ["github"],
            "resolved": ["github"],
            "matches": [{"name": "github", "schema_digest": valid_digest}],
            "missing": []
        });
        assert_eq!(
            deferred_tool_activations_from_tool_search_output(&base.to_string()).len(),
            1
        );

        let mut forged = base.clone();
        forged["requested"] = json!(["web_fetch"]);
        assert!(deferred_tool_activations_from_tool_search_output(&forged.to_string()).is_empty());

        let mut missing_resolved = base.clone();
        missing_resolved
            .as_object_mut()
            .expect("selection envelope is an object")
            .remove("resolved");
        assert!(
            deferred_tool_activations_from_tool_search_output(&missing_resolved.to_string())
                .is_empty(),
            "a selection without producer-resolved names must not authorize a carrier"
        );

        let mut malformed_digest = base;
        malformed_digest["matches"][0]["schema_digest"] = json!("sha256:not-a-digest");
        assert!(
            deferred_tool_activations_from_tool_search_output(&malformed_digest.to_string())
                .is_empty()
        );
    }

    #[test]
    fn carrier_invocation_accepts_one_canonical_target_and_object_arguments() {
        assert_eq!(
            parse_deferred_tool_invocation(&json!({
                "name": " web_fetch ",
                "arguments": {"url": "https://example.invalid"}
            })),
            Ok(DeferredToolInvocation {
                name: "web_fetch".to_string(),
                arguments: json!({"url": "https://example.invalid"}),
            })
        );
    }

    #[test]
    fn carrier_invocation_rejects_hidden_authority_and_malformed_targets() {
        for args in [
            json!({"name": "bash", "arguments": {}, "route": "edge"}),
            json!({"name": "bash", "arguments": []}),
            json!({"name": " ", "arguments": {}}),
            json!({"name": "bash"}),
        ] {
            assert!(parse_deferred_tool_invocation(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn ordinary_invocation_keeps_one_identity_without_activation() {
        let call = json!({
            "id": "provider-call-ordinary",
            "type": "function",
            "function": {"name": "read_file", "arguments": r#"{"path":"README.md"}"#}
        });
        let invocation = CanonicalToolInvocation::ordinary(call.clone());

        assert_eq!(invocation.physical_provider_call(), &call);
        assert_eq!(invocation.logical_target_call(), &call);
        assert_eq!(
            invocation.provider_call_id(),
            Some("provider-call-ordinary")
        );
        assert_eq!(invocation.activation(), None);
    }

    #[test]
    fn runtime_control_invocation_keeps_typed_host_provenance() {
        let call = json!({
            "id": "server-work-admission-t1-r0",
            "type": "function",
            "function": {"name": "start_work", "arguments": "{}"}
        });
        let invocation = CanonicalToolInvocation::runtime_control(
            call.clone(),
            RuntimeControlInvocationKind::WorkEstablishment,
        )
        .expect("typed operation matches the host-created call");

        assert_eq!(invocation.physical_provider_call(), &call);
        assert_eq!(invocation.logical_target_call(), &call);
        assert_eq!(invocation.activation(), None);
        assert_eq!(
            invocation.runtime_control_kind(),
            Some(RuntimeControlInvocationKind::WorkEstablishment)
        );
        assert!(
            CanonicalToolInvocation::runtime_control(
                call,
                RuntimeControlInvocationKind::WorkScheduler,
            )
            .is_err()
        );
    }

    #[test]
    fn runtime_control_carrier_is_bound_to_the_exact_typed_transition() {
        let carrier = json!({
            "id": "provider-settlement-1",
            "type": "function",
            "function": {
                "name": DEFERRED_TOOL_INVOCATION_CARRIER,
                "arguments": r#"{"name":"settle_work_item","arguments":{"outcome":"delivered","summary":"done"}}"#,
            }
        });
        let invocation = CanonicalToolInvocation::runtime_control_from_carrier(
            &carrier,
            RuntimeControlInvocationKind::WorkSettlement,
        )
        .expect("canonical carrier")
        .expect("exact settlement target");

        assert_eq!(invocation.physical_provider_call(), &carrier);
        assert_eq!(
            crate::tool::args::shape::tool_call_name(invocation.logical_target_call()),
            Some("settle_work_item")
        );
        assert_eq!(
            invocation.runtime_control_kind(),
            Some(RuntimeControlInvocationKind::WorkSettlement)
        );
        assert!(
            CanonicalToolInvocation::runtime_control_from_carrier(
                &carrier,
                RuntimeControlInvocationKind::WorkScheduler,
            )
            .expect("well-formed non-matching carrier")
            .is_none(),
            "one state boundary cannot authorize another lifecycle transition"
        );
    }

    #[test]
    fn carrier_canonicalization_preserves_provider_identity_and_reuses_target_path() {
        let digest = format!("sha256:{}", "c".repeat(64));
        let carrier = json!({
            "id": "provider-call-1",
            "type": "function",
            "function": {
                "name": DEFERRED_TOOL_INVOCATION_CARRIER,
                "arguments": r#"{"name":"web_fetch","arguments":{"url":"https://example.invalid"}}"#,
            }
        });
        let invocation = canonicalize_deferred_tool_invocation(
            &carrier,
            &[DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: digest.clone(),
                descriptor: None,
            }],
            |name| (name == "web_fetch").then_some(digest.clone()),
        )
        .expect("selected current target rewrites")
        .expect("carrier rewrites");

        assert_eq!(
            invocation.physical_provider_call(),
            &carrier,
            "provider identity remains intact for transcript/cache pairing"
        );
        assert_eq!(invocation.logical_target_call()["id"], "provider-call-1");
        assert_eq!(
            invocation.logical_target_call()["function"]["name"],
            "web_fetch"
        );
        assert_eq!(
            crate::tool::args::shape::parse_tool_call_arguments(invocation.logical_target_call()),
            Ok(json!({"url": "https://example.invalid"}))
        );
    }

    #[test]
    fn carrier_canonicalization_fails_closed_for_unactivated_or_stale_target() {
        let carrier = json!({
            "id": "provider-call-2",
            "type": "function",
            "function": {
                "name": DEFERRED_TOOL_INVOCATION_CARRIER,
                "arguments": r#"{"name":"web_fetch","arguments":{}}"#,
            }
        });
        assert_eq!(
            canonicalize_deferred_tool_invocation(&carrier, &[], |_| None),
            Err(DeferredToolInvocationError::NotActivated)
        );
        assert_eq!(
            canonicalize_deferred_tool_invocation(
                &carrier,
                &[DeferredToolActivation {
                    name: "web_fetch".to_string(),
                    schema_digest: format!("sha256:{}", "d".repeat(64)),
                    descriptor: None,
                }],
                |_| Some(format!("sha256:{}", "e".repeat(64))),
            ),
            Err(DeferredToolInvocationError::ActivationStale)
        );
    }

    #[test]
    fn carrier_canonicalization_rejects_self_target_even_with_forged_evidence() {
        let digest = format!("sha256:{}", "e".repeat(64));
        let carrier = json!({
            "id": "provider-call-self",
            "type": "function",
            "function": {
                "name": DEFERRED_TOOL_INVOCATION_CARRIER,
                "arguments": r#"{"name":"invoke_tool","arguments":{}}"#,
            }
        });
        assert_eq!(
            canonicalize_deferred_tool_invocation(
                &carrier,
                &[DeferredToolActivation {
                    name: DEFERRED_TOOL_INVOCATION_CARRIER.to_string(),
                    schema_digest: digest.clone(),
                    descriptor: None,
                }],
                |_| Some(digest.clone()),
            ),
            Err(DeferredToolInvocationError::NotActivated)
        );
    }

    #[test]
    fn carrier_canonicalization_rejects_ambiguous_activation_revisions() {
        let carrier = json!({
            "id": "provider-call-3",
            "type": "function",
            "function": {
                "name": DEFERRED_TOOL_INVOCATION_CARRIER,
                "arguments": r#"{"name":"web_fetch","arguments":{}}"#,
            }
        });
        assert_eq!(
            canonicalize_deferred_tool_invocation(
                &carrier,
                &[
                    DeferredToolActivation {
                        name: "web_fetch".to_string(),
                        schema_digest: format!("sha256:{}", "f".repeat(64)),
                        descriptor: None,
                    },
                    DeferredToolActivation {
                        name: "web_fetch".to_string(),
                        schema_digest: format!("sha256:{}", "0".repeat(64)),
                        descriptor: None,
                    },
                ],
                |_| Some(format!("sha256:{}", "f".repeat(64))),
            ),
            Err(DeferredToolInvocationError::ActivationStale)
        );
    }

    #[test]
    fn batch_resolution_keeps_independent_direct_calls_when_carrier_is_stale() {
        let calls = vec![
            json!({"id":"direct-1","type":"function","function":{"name":"read_file","arguments":"{}"}}),
            json!({"id":"carrier-1","type":"function","function":{"name":"invoke_tool","arguments":"{\"name\":\"web_fetch\",\"arguments\":{}}"}}),
        ];
        let resolved = canonicalize_tool_invocation_batch(
            &calls,
            &[DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: format!("sha256:{}", "a".repeat(64)),
                descriptor: None,
            }],
            |_| Some(format!("sha256:{}", "b".repeat(64))),
        );

        assert_eq!(resolved.len(), 2);
        assert_eq!(
            resolved[0]
                .as_ref()
                .expect("direct call survives")
                .logical_target_call()["function"]["name"],
            "read_file"
        );
        assert_eq!(
            resolved[1],
            Err(DeferredToolInvocationError::ActivationStale)
        );
    }

    #[test]
    fn refreshing_carrier_activation_replaces_the_previous_schema_revision() {
        let mut activations = vec![DeferredToolActivation {
            name: "web_fetch".to_string(),
            schema_digest: format!("sha256:{}", "1".repeat(64)),
            descriptor: None,
        }];
        refresh_deferred_tool_activations(
            &mut activations,
            [DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: format!("sha256:{}", "2".repeat(64)),
                descriptor: None,
            }],
        );

        assert_eq!(
            activations,
            vec![DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: format!("sha256:{}", "2".repeat(64)),
                descriptor: None,
            }]
        );
    }

    #[test]
    fn canonical_history_reconstructs_typed_activation_and_replaces_revision() {
        let selection = |digest: char| {
            json!({
                "mode": "select",
                "query": "select:web_fetch",
                "requested": ["web_fetch"],
                "resolved": ["web_fetch"],
                "matches": [{
                    "name": "web_fetch",
                    "schema_digest": format!("sha256:{}", digest.to_string().repeat(64))
                }],
                "missing": []
            })
            .to_string()
        };
        let messages = vec![
            json!({"role": "assistant", "tool_calls": [{"id": "search-1", "function": {"name": "tool_search", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "search-1", "content": selection('a')}),
            json!({"role": "assistant", "tool_calls": [{"id": "search-2", "function": {"name": "tool_search", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "search-2", "content": selection('b')}),
        ];

        assert_eq!(
            deferred_tool_activations_from_messages(&messages),
            vec![DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: format!("sha256:{}", "b".repeat(64)),
                descriptor: None,
            }]
        );
    }

    #[test]
    fn canonical_history_ignores_unpaired_or_non_selection_search_results() {
        let selected = json!({
            "mode": "select",
            "query": "select:github",
            "requested": ["github"],
            "matches": [{"name": "github"}],
            "missing": []
        })
        .to_string();
        let non_selection = json!({
            "mode": "error",
            "status": "failed",
            "query": "github",
            "matches": [{"name": "github"}]
        })
        .to_string();
        let messages = vec![
            json!({"role": "tool", "tool_call_id": "search-1", "content": selected}),
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "search-1",
                    "function": {"name": "tool_search", "arguments": "{}"}
                }]
            }),
            json!({"role": "tool", "tool_call_id": "search-1", "content": non_selection}),
            json!({
                "role": "tool",
                "tool_call_id": "missing-call",
                "content": {
                    "mode": "select",
                    "query": "select:github",
                    "requested": ["github"],
                    "matches": [{"name": "github"}],
                    "missing": []
                }
            }),
        ];

        assert!(deferred_tool_activations_from_messages(&messages).is_empty());
    }

    #[test]
    fn direct_deferred_call_with_runtime_binding_activates() {
        assert_eq!(
            classify_direct_deferred_call("run_script", true, |n| n == "run_script"),
            DirectDeferredCallAdmission::Activate {
                name: "run_script".to_string()
            },
            "a deferred tool with a runtime binding must be treated as an activation intent"
        );
    }

    #[test]
    fn direct_deferred_call_without_runtime_binding_is_not_admitted() {
        assert_eq!(
            classify_direct_deferred_call("run_script", true, |_| false),
            DirectDeferredCallAdmission::NotAdmitted,
            "a deferred tool without a runtime binding cannot be activated"
        );
    }

    #[test]
    fn direct_call_to_name_not_in_deferred_manifest_is_unknown() {
        assert_eq!(
            classify_direct_deferred_call("hallucinated_tool", false, |_| true),
            DirectDeferredCallAdmission::Unknown,
            "a name not advertised in <deferred-tools> must not be activated even if a binding exists"
        );
    }

    #[test]
    fn direct_deferred_call_activation_message_names_the_tool_and_forbids_execution() {
        let msg = direct_deferred_call_activation_message("run_script");
        assert!(msg.contains("run_script"), "message must name the tool");
        assert!(
            msg.contains("select:run_script"),
            "message must frame the call as a select intent"
        );
        assert!(
            msg.contains("not executed"),
            "message must state the args were not executed"
        );
        assert!(
            msg.contains("arguments were ignored"),
            "message must state direct-call args are not reused"
        );
        assert!(
            msg.contains("Do not call `run_script` again in the same tool-call batch"),
            "message must prevent same-batch retry loops"
        );
        assert!(
            msg.contains("later model request"),
            "message must require a later request with the full schema"
        );
    }

    #[test]
    fn deferred_tool_not_activatable_message_avoids_search_retry_loop() {
        let msg = deferred_tool_not_activatable_message("github");

        assert!(msg.contains("<deferred-tools>"), "{msg}");
        assert!(msg.contains("not activatable"), "{msg}");
        assert!(msg.contains("Do not retry"), "{msg}");
        assert!(
            msg.contains("tool_search(query=\"select:github\")"),
            "{msg}"
        );
    }

    #[test]
    fn paired_batch_preserves_provider_order_and_identity_across_views() {
        let direct = CanonicalToolInvocation::ordinary(json!({
            "id": "direct-1", "type": "function",
            "function": {"name": "read_file", "arguments": "{}"}
        }));
        let carrier = json!({
            "id": "carrier-2", "type": "function",
            "function": {"name": "invoke_tool", "arguments": "{\"name\":\"web_fetch\",\"arguments\":{}}"}
        });
        let activation = DeferredToolActivation {
            name: "web_fetch".to_string(),
            schema_digest: "sha256:current".to_string(),
            descriptor: None,
        };
        let deferred = canonicalize_deferred_tool_invocation(&carrier, &[activation], |_| {
            Some("sha256:current".to_string())
        })
        .expect("carrier is valid")
        .expect("carrier resolves");
        let batch = CanonicalToolInvocationBatch::new(vec![direct, deferred]);

        assert!(batch.validate().is_ok());
        assert_eq!(
            batch
                .physical_provider_calls()
                .iter()
                .filter_map(|call| crate::tool::args::shape::tool_call_name(call))
                .collect::<Vec<_>>(),
            vec!["read_file", "invoke_tool"]
        );
        assert_eq!(
            batch
                .logical_target_calls()
                .iter()
                .filter_map(|call| crate::tool::args::shape::tool_call_name(call))
                .collect::<Vec<_>>(),
            vec!["read_file", "web_fetch"]
        );
    }

    #[test]
    fn merge_keeps_schema_addressed_evidence_and_replaces_a_stale_revision() {
        let merged = merged_deferred_tool_activations(
            &[],
            [
                DeferredToolActivation {
                    name: "web_fetch".to_string(),
                    schema_digest: "sha256:old".to_string(),
                    descriptor: None,
                },
                DeferredToolActivation {
                    name: "web_fetch".to_string(),
                    schema_digest: "sha256:current".to_string(),
                    descriptor: None,
                },
                DeferredToolActivation {
                    name: "session".to_string(),
                    schema_digest: "sha256:session".to_string(),
                    descriptor: None,
                },
            ],
        );

        assert_eq!(
            merged,
            vec![
                DeferredToolActivation {
                    name: "session".to_string(),
                    schema_digest: "sha256:session".to_string(),
                    descriptor: None,
                },
                DeferredToolActivation {
                    name: "web_fetch".to_string(),
                    schema_digest: "sha256:current".to_string(),
                    descriptor: None,
                },
            ]
        );
    }

    #[test]
    fn merge_does_not_downgrade_a_provider_bound_activation_from_history() {
        let descriptor = astra_turn_types::ResolvedToolDescriptorRef::new(
            astra_turn_types::ToolIdentity::new(
                astra_turn_types::ProviderBindingRef::new("provider-current").unwrap(),
                astra_turn_types::NativeToolId::new("web_fetch").unwrap(),
            ),
            "sha256:provider-descriptor",
        )
        .unwrap();
        let messages = vec![
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "search-1",
                    "type": "function",
                    "function": {
                        "name": "tool_search",
                        "arguments": "{\"query\":\"select:web_fetch\"}"
                    }
                }]
            }),
            json!({
                "role": "tool",
                "tool_call_id": "search-1",
                "content": serde_json::json!({
                    "mode": "select",
                    "query": "select:web_fetch",
                    "requested": ["web_fetch"],
                    "resolved": ["web_fetch"],
                    "matches": [{
                        "name": "web_fetch",
                        "schema_digest": format!("sha256:{}", "a".repeat(64))
                    }],
                    "missing": []
                })
                .to_string()
            }),
        ];

        let merged = merged_deferred_tool_activations(
            &messages,
            [DeferredToolActivation {
                name: "web_fetch".to_string(),
                schema_digest: "sha256:provider-schema".to_string(),
                descriptor: Some(descriptor.clone()),
            }],
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].descriptor, Some(descriptor));
        assert_eq!(merged[0].schema_digest, "sha256:provider-schema");
    }
}
