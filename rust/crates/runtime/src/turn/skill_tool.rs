//! Skill tool — allows the LLM to invoke registered skills as tool calls.
//!
//! # Architecture
//!
//! The skill system follows the same interception pattern as delegation:
//!
//! 1. **Schema injection**: If a [`SkillResolver`] is wired on [`AgenticLoopState`],
//!    the loop injects a `skill` tool schema listing available skills.
//!
//! 2. **Call interception**: When the LLM emits a `skill` tool call,
//!    [`partition_and_execute_skills`] intercepts it before the headless tool round.
//!
//! 3. **Resolution**: The [`SkillResolver`] loads the skill instructions and returns
//!    them as the tool result, so the LLM follows those instructions in the
//!    current conversation.
//!
//! # Host Implementations
//!
//! | Host | Crate | SkillResolver |
//! |------|-------|---------------|
//! | CLI  | astra-cli | Wraps `SkillRegistry` from `skill_instructions.rs` |
//! | Server | runtime/server | (Future) wraps cloud skill catalog |
//!
//! # Fork (isolated) execution
//!
//! Skills with [`ExecutionContext::Fork`](crate::skills::manifest::ExecutionContext::Fork)
//! run in a nested agentic loop via [`crate::skills::executor::IsolatedSkillExecutor`] and a
//! host-provided [`crate::skills::executor::SkillSubRunExecutor`] (e.g. CLI fork sub-run host).
//! Recursive fork is disabled in sub-runs (`skill_executor: None`); nested `skill` calls are
//! still resolved and executed **inline** so composition can proceed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, OnceLock};

use astra_core::SkillSearchSettings;
use serde_json::Value;

use crate::server::header_utils::CONNECTION_HEADER_TOKENS_KEY;
use crate::skills::arguments::substitute_arguments;
use crate::skills::hooks::HookAction;
use crate::skills::manifest::{
    EffortLevel, ExecutionContext, LoadedSkill, SkillManifest, SkillSourceKind,
};
use crate::skills::traits::{SkillExecutionContext, SkillExecutor};

// ─── Skill resolution trait ──────────────────────────────────────────────────

/// Lightweight description of a skill for tool schema generation.
#[derive(Clone, Debug)]
pub struct SkillToolInfo {
    pub name: String,
    pub description: String,
    /// Natural-language hint for when the model should pick this skill.
    pub when_to_use: Option<String>,
    /// Where this skill was loaded from (bundled skills get priority in budget).
    pub source: SkillSourceKind,
    /// Alternative names for this skill.
    pub aliases: Vec<String>,
    /// Optional category from manifest (e.g. `code-review`).
    pub category: Option<String>,
    /// Free-form tags from manifest.
    pub tags: Vec<String>,
    /// Trigger phrases / keywords from manifest.
    pub triggers: Vec<String>,
}

impl Default for SkillToolInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            when_to_use: None,
            source: SkillSourceKind::Local,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
            triggers: Vec::new(),
        }
    }
}

// ─── Skill context ───────────────────────────────────────────────────────────

/// Runtime context available to skills during execution.
///
/// Injected into skill instructions via `${CTX_*}` placeholders.
/// Built at execution time from the agentic loop state.
#[derive(Clone, Default)]
pub struct SkillContext {
    /// Current session identifier.
    pub session_id: Option<String>,
    /// Directory where session artifacts are stored.
    pub session_dir: Option<String>,
    /// Current working directory of the agent.
    pub work_dir: Option<String>,
    /// Names of tools available to the agent in this turn.
    pub available_tools: Vec<String>,
    /// Current nested agent/sub-run depth of the caller.
    pub recursion_depth: u8,
    /// Inbound request headers eligible for remote skill forwarding.
    /// Header names are normalized to lowercase.
    pub forward_headers: HashMap<String, String>,
    /// Extensible key-value pairs for host-specific context.
    pub extra: HashMap<String, String>,
}

fn redacted_forward_header_names(headers: &HashMap<String, String>) -> Vec<&str> {
    let mut names = headers
        .keys()
        .filter(|name| !name.starts_with("__astra_"))
        .map(String::as_str)
        .collect::<Vec<_>>();
    names.sort_unstable();
    names
}

struct RedactedForwardHeadersDebug<'a>(&'a HashMap<String, String>);

impl fmt::Debug for RedactedForwardHeadersDebug<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = redacted_forward_header_names(self.0);
        f.debug_struct("RedactedForwardHeaders")
            .field("count", &names.len())
            .field("names", &names)
            .finish()
    }
}

impl fmt::Debug for SkillContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkillContext")
            .field("session_id", &self.session_id)
            .field("session_dir", &self.session_dir)
            .field("work_dir", &self.work_dir)
            .field("available_tools", &self.available_tools)
            .field("recursion_depth", &self.recursion_depth)
            .field(
                "forward_headers",
                &RedactedForwardHeadersDebug(&self.forward_headers),
            )
            .field("extra", &self.extra)
            .finish()
    }
}

impl SkillContext {
    /// Convert context fields into a flat `HashMap` for argument substitution.
    ///
    /// Keys are prefixed with `CTX_` to avoid collisions with user-defined arguments.
    pub fn as_substitution_vars(&self) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        if let Some(ref id) = self.session_id {
            vars.insert("CTX_SESSION_ID".into(), id.clone());
        }
        if let Some(ref dir) = self.session_dir {
            vars.insert("CTX_SESSION_DIR".into(), dir.clone());
        }
        if let Some(ref dir) = self.work_dir {
            vars.insert("CTX_WORK_DIR".into(), dir.clone());
        }
        if !self.available_tools.is_empty() {
            vars.insert(
                "CTX_AVAILABLE_TOOLS".into(),
                self.available_tools.join(", "),
            );
        }
        for (k, v) in &self.extra {
            vars.insert(format!("CTX_{}", k.to_uppercase()), v.clone());
        }
        vars
    }
}

/// Record of a skill invoked during this session.
/// Used for same-session dedup and post-compaction re-injection.
#[derive(Clone, Debug)]
pub struct InvokedSkill {
    /// Canonical skill name.
    pub name: String,
    /// Full instructions returned on first invocation.
    pub content: String,
    /// Turn number when the skill was first invoked.
    pub invoked_at_turn: u32,
}

/// A fully resolved skill ready for execution.
#[derive(Clone, Debug)]
pub struct ResolvedSkill {
    pub name: String,
    pub instructions: String,
    /// Model override (e.g. `"claude-sonnet-4-20250514"`).
    pub model: Option<String>,
    /// Token budget (0 or None = system default).
    pub max_tokens: Option<u32>,
    /// Tool allowlist (empty = all tools).
    pub allowed_tools: Vec<String>,
    /// Execution context — inline (inject into conversation) or fork (sub-agent).
    pub execution_context: ExecutionContext,
    /// Lifecycle hooks (pre/post invocation).
    pub hooks: crate::skills::hooks::SkillHooks,
    /// Skill directory path for `${SKILL_DIR}` substitution.
    pub skill_dir: Option<String>,
    /// Where this skill was loaded from. Used for security sandboxing
    /// (e.g. MCP skills cannot run inline shell or hooks).
    pub source: SkillSourceKind,
    /// Machine-executable success criteria (empty = no verification).
    pub success_criteria: Vec<astra_services::VerificationCriterion>,
    /// Composition metadata (None = not declared, treated as non-composable in nested context).
    pub composition: Option<crate::skills::manifest::SkillComposition>,
    /// Input schema for argument validation (JSON Schema subset).
    pub input_schema: Option<Value>,
    /// Output schema for execution-result validation (JSON Schema subset).
    pub output_schema: Option<Value>,
    /// Remote execution endpoint. When set, runtime dispatches over HTTP.
    pub remote_url: Option<String>,
    /// Header names to forward from inbound request headers to remote callback.
    pub forward_headers: Vec<String>,
    /// Header names required to be present before remote callback is attempted.
    pub required_headers: Vec<String>,
    /// Alternative names for this skill.
    pub aliases: Vec<String>,
    /// Effort level hint for reasoning depth.
    pub effort: Option<crate::skills::manifest::EffortLevel>,
    /// Agent type for fork execution (e.g. "general-purpose").
    pub agent_type: Option<String>,
    /// Trust tier — determines sandbox policy during execution.
    pub trust_tier: crate::skills::manifest::TrustTier,
}

/// Trait for resolving skill names to instructions.
///
/// Implementations live in host crates (astra-cli, server) since the runtime
/// crate cannot depend on them.
pub trait SkillResolver: Send + Sync {
    /// Resolve a skill by name, loading instructions if needed.
    fn resolve(&self, name: &str) -> Result<ResolvedSkill, String>;

    /// List available skills for schema generation.
    fn available_skills(&self) -> Vec<SkillToolInfo>;
}

// ─── Tool schema ─────────────────────────────────────────────────────────────

pub const SKILL_TOOL_NAME: &str = "skill";

/// Second-stage discovery tool for skill search.
pub const DISCOVER_SKILLS_TOOL_NAME: &str = "discover_skills";

/// Max skills returned from a single `discover_skills` call.
const DISCOVER_SKILLS_MAX_RESULTS: usize = 8;

/// Character budget for the skill listing section (1% of 200k tokens × 4 chars/token).
const DEFAULT_SKILL_LISTING_BUDGET: usize = 8_000;

/// Per-entry description cap. Listing is for discovery only — the full content
/// is loaded when a skill is actually invoked.
const MAX_LISTING_DESC_CHARS: usize = 250;

/// Truncate a string at a byte budget, respecting UTF-8 char boundaries.
fn truncate_desc(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes.saturating_sub(1);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Format a single skill's description, respecting the per-entry cap.
fn format_skill_description(s: &SkillToolInfo) -> String {
    let mut desc = match &s.when_to_use {
        Some(when) => format!("{} (use when: {})", s.description, when),
        None => s.description.clone(),
    };
    if !s.aliases.is_empty() {
        desc.push_str(&format!(" [aliases: {}]", s.aliases.join(", ")));
    }
    if desc.len() > MAX_LISTING_DESC_CHARS {
        truncate_desc(&desc, MAX_LISTING_DESC_CHARS)
    } else {
        desc
    }
}

/// Format a skill list with token budget. Bundled skills always keep full
/// descriptions; other skills get truncated or reduced to names-only when
/// the budget is tight.
///
/// Returns `(entries, all_names)` where `entries` are "- name: desc" lines
/// and `all_names` are all skill names (including those reduced to name-only).
fn format_skills_within_budget(
    skills: &[SkillToolInfo],
    budget: usize,
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
    pinned_skills: Option<&std::collections::HashSet<String>>,
) -> (Vec<String>, Vec<String>) {
    if skills.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut seen: std::collections::HashSet<&str> =
        skills.iter().map(|s| s.name.as_str()).collect();
    let mut all_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    // Include aliases so the LLM can invoke skills by alternative names
    for s in skills {
        for alias in &s.aliases {
            if seen.insert(alias.as_str()) {
                all_names.push(alias.clone());
            }
        }
    }

    // Try full descriptions first
    let full_entries: Vec<String> = skills
        .iter()
        .map(|s| format!("- **{}**: {}", s.name, format_skill_description(s)))
        .collect();
    let total: usize = full_entries.iter().map(|e| e.len() + 1).sum();

    if total <= budget {
        return (full_entries, all_names);
    }

    // Partition into priority (bundled + pinned, never truncated) and rest
    let mut bundled_entries = Vec::new();
    let mut rest_skills: Vec<&SkillToolInfo> = Vec::new();
    for (i, s) in skills.iter().enumerate() {
        let is_pinned = pinned_skills.map_or(false, |p| p.contains(&s.name));
        if s.source == SkillSourceKind::Bundled || is_pinned {
            bundled_entries.push(full_entries[i].clone());
        } else {
            rest_skills.push(s);
        }
    }

    // Sort non-bundled by quality boost (highest first) for priority in budget
    if let Some(tracker) = quality_tracker {
        rest_skills.sort_by(|a, b| {
            tracker
                .selection_boost(&b.name)
                .partial_cmp(&tracker.selection_boost(&a.name))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    let bundled_chars: usize = bundled_entries.iter().map(|e| e.len() + 1).sum();
    let remaining_budget = budget.saturating_sub(bundled_chars);

    if rest_skills.is_empty() {
        return (bundled_entries, all_names);
    }

    // Calculate max per-entry description length for non-bundled
    let name_overhead: usize = rest_skills
        .iter()
        .map(|s| s.name.len() + 6) // "- **name**: " + newline
        .sum();
    let avail = remaining_budget.saturating_sub(name_overhead);
    let max_desc = avail / rest_skills.len();

    let mut entries = bundled_entries;
    if max_desc < 20 {
        // Extreme pressure: non-bundled go names-only
        for s in &rest_skills {
            entries.push(format!("- {}", s.name));
        }
    } else {
        for s in &rest_skills {
            let desc = format_skill_description(s);
            let truncated = if desc.len() > max_desc {
                truncate_desc(&desc, max_desc)
            } else {
                desc
            };
            entries.push(format!("- **{}**: {}", s.name, truncated));
        }
    }

    (entries, all_names)
}

fn tokenize_query(q: &str) -> Vec<String> {
    q.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| s.len() > 1)
        .map(std::string::ToString::to_string)
        .collect()
}

fn haystack_for_scoring(s: &SkillToolInfo) -> String {
    let mut h = format!("{} {}", s.name, s.description);
    if let Some(w) = &s.when_to_use {
        h.push(' ');
        h.push_str(w);
    }
    if let Some(c) = &s.category {
        h.push(' ');
        h.push_str(c);
    }
    for t in &s.tags {
        h.push(' ');
        h.push_str(t);
    }
    for t in &s.triggers {
        h.push(' ');
        h.push_str(t);
    }
    for a in &s.aliases {
        h.push(' ');
        h.push_str(a);
    }
    h.to_lowercase()
}

fn score_skill_for_query(
    s: &SkillToolInfo,
    query_lower: &str,
    query_tokens: &[String],
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
) -> f32 {
    let mut score: f32 = 0.0;
    let hay = haystack_for_scoring(s);
    let name_l = s.name.to_lowercase();

    if !query_lower.is_empty() {
        if name_l == query_lower {
            score += 12.0;
        } else if hay.contains(query_lower) {
            score += 6.0;
        }
        if query_lower.contains(&name_l) || name_l.contains(query_lower) {
            score += 4.0;
        }
    }

    for t in query_tokens {
        if name_l == *t || s.aliases.iter().any(|a| a.to_lowercase() == *t) {
            score += 5.0;
        } else if s.triggers.iter().any(|tr| tr.to_lowercase().contains(t)) {
            score += 4.0;
        } else if hay.contains(t) {
            score += 1.5;
        }
    }

    if matches!(s.source, SkillSourceKind::Bundled) {
        score += 1.25;
    }

    if let Some(qt) = quality_tracker {
        score += qt.selection_boost(&s.name) as f32 * 0.5;
    }

    score
}

/// Pick a small relevant subset for the current user message when dynamic surfacing applies.
pub fn select_skills_for_turn(
    all_skills: &[SkillToolInfo],
    user_message: &str,
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
    pinned_skills: Option<&HashSet<String>>,
    cfg: &SkillSearchSettings,
) -> Vec<SkillToolInfo> {
    if cfg.use_full_catalog(all_skills.len()) {
        return all_skills.to_vec();
    }

    let mut picked: Vec<SkillToolInfo> = Vec::new();
    let mut picked_names: HashSet<String> = HashSet::new();
    if let Some(pinned) = pinned_skills {
        for s in all_skills {
            if pinned.contains(&s.name) {
                picked_names.insert(s.name.clone());
                picked.push(s.clone());
            }
        }
    }

    let query_lower = user_message.trim().to_lowercase();
    let tokens = tokenize_query(user_message);

    let mut scored: Vec<(f32, &SkillToolInfo)> = all_skills
        .iter()
        .filter(|s| !picked_names.contains(&s.name))
        .map(|s| {
            let sc = score_skill_for_query(s, &query_lower, &tokens, quality_tracker);
            (sc, s)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let threshold = 0.8_f32;
    let top_score = scored.first().map(|(s, _)| *s).unwrap_or(0.0);
    let weak = top_score < threshold;

    if !weak {
        for (sc, s) in scored {
            if picked.len() >= cfg.surface_cap {
                break;
            }
            if sc >= threshold {
                picked_names.insert(s.name.clone());
                picked.push(s.clone());
            }
        }
    }

    if weak || picked.len() < 3 {
        picked.clear();
        picked_names.clear();
        if let Some(pinned) = pinned_skills {
            for s in all_skills {
                if pinned.contains(&s.name) {
                    picked_names.insert(s.name.clone());
                    picked.push(s.clone());
                }
            }
        }
        let mut rest: Vec<&SkillToolInfo> = all_skills
            .iter()
            .filter(|s| !picked_names.contains(&s.name))
            .collect();
        rest.sort_by(|a, b| {
            let pa = matches!(a.source, SkillSourceKind::Bundled);
            let pb = matches!(b.source, SkillSourceKind::Bundled);
            pb.cmp(&pa).then_with(|| {
                let qa = quality_tracker.map_or(0.0, |q| q.selection_boost(&a.name));
                let qb = quality_tracker.map_or(0.0, |q| q.selection_boost(&b.name));
                qb.partial_cmp(&qa).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        for s in rest {
            if picked.len() >= cfg.surface_cap {
                break;
            }
            picked.push((*s).clone());
        }
    }

    picked
}

/// Skills visible this session: auto-surface ∪ user-pinned ∪ previously discovered.
pub fn merge_discovered_skills_into_visible(
    base: Vec<SkillToolInfo>,
    all_skills: &[SkillToolInfo],
    discovered: &HashSet<String>,
) -> Vec<SkillToolInfo> {
    let mut out = base;
    let mut have: HashSet<String> = out.iter().map(|s| s.name.clone()).collect();
    for s in all_skills {
        if discovered.contains(&s.name) && have.insert(s.name.clone()) {
            out.push(s.clone());
        }
    }
    out
}

/// Visible skills + whether dynamic surfacing is active (open `skill_name` + `discover_skills`).
pub fn visible_skills_for_host_turn(
    full: &[SkillToolInfo],
    user_message: &str,
    quality_tracker: &crate::skills::quality::SkillQualityTracker,
    pinned: &HashSet<String>,
    discovered: &HashSet<String>,
    cfg: &SkillSearchSettings,
) -> (Vec<SkillToolInfo>, bool) {
    if cfg.use_full_catalog(full.len()) {
        return (full.to_vec(), false);
    }
    let base = select_skills_for_turn(full, user_message, Some(quality_tracker), Some(pinned), cfg);
    let visible = merge_discovered_skills_into_visible(base, full, discovered);
    (visible, true)
}

/// Lowercased canonical names and aliases — used to filter `discover_skills` results.
pub fn skill_mask_names_lowercase(skills: &[SkillToolInfo]) -> HashSet<String> {
    let mut m = HashSet::new();
    for s in skills {
        m.insert(s.name.to_lowercase());
        for a in &s.aliases {
            m.insert(a.to_lowercase());
        }
    }
    m
}

/// OpenAI-style tool schema for `discover_skills`.
pub fn discover_skills_tool_schema() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": DISCOVER_SKILLS_TOOL_NAME,
            "description": "Search the full skill catalog for additional workflow packs not shown in the current skill listing.\n\n\
                Call this when you are pivoting, planning a multi-step workflow, or the surfaced skills do not cover your next action. \
                Skills already listed for this turn (or discovered earlier in the session) are filtered out.\n\n\
                After a successful discovery, invoke `skill` with one of the returned names.",
            "parameters": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Concrete description of what you are trying to do next (task, domain, or workflow)."
                    }
                }
            }
        }
    })
}

/// True if this tool call targets `discover_skills`.
pub fn is_discover_skills_call(tool_call: &Value) -> bool {
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        == Some(DISCOVER_SKILLS_TOOL_NAME)
}

/// Run discovery; returns assistant-facing text and canonical names to merge into session state.
pub fn execute_discover_skills(
    query: &str,
    catalog: &[SkillToolInfo],
    mut excluded_lowercase: HashSet<String>,
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
) -> (String, Vec<String>) {
    let query_lower = query.trim().to_lowercase();
    let tokens = tokenize_query(query);

    let mut candidates: Vec<(&SkillToolInfo, f32)> = catalog
        .iter()
        .filter(|s| {
            !excluded_lowercase.contains(&s.name.to_lowercase())
                && !s
                    .aliases
                    .iter()
                    .any(|a| excluded_lowercase.contains(&a.to_lowercase()))
        })
        .map(|s| {
            let sc = score_skill_for_query(s, &query_lower, &tokens, quality_tracker);
            (s, sc)
        })
        .filter(|(_, sc)| *sc > 0.01)
        .collect();

    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    if candidates.is_empty() {
        return (
            "No additional skills matched that query. Try different keywords, or proceed with general tools.".to_string(),
            Vec::new(),
        );
    }

    let mut lines = Vec::new();
    let mut new_names = Vec::new();
    for (s, _) in candidates.iter().take(DISCOVER_SKILLS_MAX_RESULTS) {
        lines.push(format!("- **{}**: {}", s.name, format_skill_description(s)));
        new_names.push(s.name.clone());
        excluded_lowercase.insert(s.name.to_lowercase());
        for a in &s.aliases {
            excluded_lowercase.insert(a.to_lowercase());
        }
    }

    let body = lines.join("\n");
    (
        format!(
            "Additional skills (now available via `skill` for this session):\n\n{body}\n\n\
             Invoke `skill` with `skill_name` set to one of the names above."
        ),
        new_names,
    )
}

/// Generate the OpenAI-compatible tool schema for the `skill` tool.
///
/// When `open_skill_name` is true (dynamic surfacing), `skill_name` is a free string
/// (no JSON `enum`) so the catalog can grow mid-session via `discover_skills` without
/// re-injecting schemas. Otherwise an enum lists all callable aliases.
///
/// Descriptions are budget-capped to avoid blowing up the context window.
pub fn skill_tool_schema(
    skills: &[SkillToolInfo],
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
    pinned_skills: Option<&std::collections::HashSet<String>>,
    open_skill_name: bool,
) -> Value {
    let (skill_entries, all_names) = format_skills_within_budget(
        skills,
        DEFAULT_SKILL_LISTING_BUDGET,
        quality_tracker,
        pinned_skills,
    );

    let mut skill_name_prop = serde_json::json!({
        "type": "string",
        "description": "The name of the skill to execute (canonical name or alias)."
    });
    if !open_skill_name {
        let skill_names: Vec<Value> = all_names.into_iter().map(Value::String).collect();
        if let Some(obj) = skill_name_prop.as_object_mut() {
            obj.insert("enum".to_string(), Value::Array(skill_names));
        }
    }

    let dynamic_note = if open_skill_name {
        "\n\nOnly a subset of skills is listed below. If none apply, call `discover_skills` with a specific description of your next action before improvising."
    } else {
        ""
    };

    let description = format!(
        "Execute a skill within the current conversation.\n\n\
         When users ask you to perform tasks, check if any of the available skills \
         below can help complete the task more effectively. Skills provide specialized \
         capabilities and domain knowledge.\n\n\
         How to invoke:\n\
         - Use this tool with the skill name only (no arguments) for most skills\n\
         - Optionally provide a task description for additional context\n\n\
         Important:\n\
         - When a skill is relevant to the user's request, invoke it IMMEDIATELY \
         as your first action\n\
         - When a skill matches the user's request, this is a BLOCKING REQUIREMENT: \
         invoke the relevant skill tool BEFORE generating any other response about the task. \
         Do NOT call any other tools in the same response as a skill invocation — the skill \
         must be loaded first so you can follow its instructions\n\
         - NEVER just mention a skill in your text response without actually calling this tool\n\
         - If the user explicitly references a skill by name, invoke it\n\
         - If you see a `<skill-loaded name=\"...\"/>` tag in a tool result, the skill \
         has already executed. Follow those instructions directly — do NOT call any \
         other tools or re-invoke the skill\n\n\
         Available skills:\n{}{}",
        skill_entries.join("\n"),
        dynamic_note
    );

    serde_json::json!({
        "type": "function",
        "function": {
            "name": SKILL_TOOL_NAME,
            "description": description,
            "parameters": {
                "type": "object",
                "required": ["skill_name"],
                "properties": {
                    "skill_name": skill_name_prop,
                    "task": {
                        "type": "string",
                        "description": "Optional task description or additional context for the skill. If omitted, the skill uses the current conversation context."
                    }
                }
            }
        }
    })
}

/// Build a system-reminder message listing available skills.
///
/// Injected into the conversation so the LLM is aware skills exist even
/// before inspecting tool schemas. Uses the same budget as the tool schema.
pub fn skill_listing_system_message(
    skills: &[SkillToolInfo],
    quality_tracker: Option<&crate::skills::quality::SkillQualityTracker>,
    pinned_skills: Option<&std::collections::HashSet<String>>,
    append_discover_hint: bool,
) -> Value {
    let (entries, _) = format_skills_within_budget(
        skills,
        DEFAULT_SKILL_LISTING_BUDGET,
        quality_tracker,
        pinned_skills,
    );

    // Build a name→category lookup for skills that have categories
    let category_by_name: std::collections::HashMap<&str, &str> = skills
        .iter()
        .filter_map(|s| s.category.as_ref().map(|c| (s.name.as_str(), c.as_str())))
        .collect();

    let mut lines = Vec::with_capacity(entries.len() + 4);
    lines.push("<available_skills>".to_string());
    for entry in &entries {
        // Convert "- **name**: desc" to XML format with optional category
        let trimmed = entry.trim_start_matches("- ");
        if let Some(colon_pos) = trimmed.find(": ") {
            let name = trimmed[..colon_pos].trim_matches('*');
            let desc = &trimmed[colon_pos + 2..];
            let category_line = category_by_name
                .get(name)
                .map(|c| format!("\n  <category>{c}</category>"))
                .unwrap_or_default();
            lines.push(format!(
                "<skill>\n  <name>{name}</name>{category_line}\n  <description>{desc}</description>\n</skill>"
            ));
        } else {
            // Names-only fallback
            let name = trimmed.trim_matches('*');
            let category_line = category_by_name
                .get(name)
                .map(|c| format!("\n  <category>{c}</category>"))
                .unwrap_or_default();
            lines.push(format!(
                "<skill>\n  <name>{name}</name>{category_line}\n</skill>"
            ));
        }
    }
    lines.push("</available_skills>".to_string());

    let discover_note = if append_discover_hint {
        "\n\nRelevant skills are surfaced each turn. If you are pivoting or none of the above \
         fit your next step, call `discover_skills` with a specific description before improvising."
    } else {
        ""
    };

    let content = format!(
        "You have access to specialized skills via the `skill` tool. \
         This is a BLOCKING REQUIREMENT: when a user's request matches a skill, \
         invoke the skill tool BEFORE generating any other response about the task. \
         Do NOT call any other tools in the same response as a skill invocation — \
         the skill must be loaded first so you can follow its instructions. \
         Do not attempt to manually replicate what a skill does — skills encode \
         domain-specific workflows that outperform ad-hoc tool calls.\n\n\
         When you see a `<skill-loaded name=\"...\"/>` tag in a tool result, the skill \
         has already executed and its instructions are in that result. Follow those \
         instructions directly — do NOT invoke any other tools or call the skill again.\n\n{}{}",
        lines.join("\n"),
        discover_note
    );

    serde_json::json!({
        "role": "system",
        "content": content
    })
}

/// Check if a tool call is a skill invocation.
pub fn is_skill_call(tool_call: &Value) -> bool {
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        == Some(SKILL_TOOL_NAME)
}

/// Parse tool call arguments from either stringified JSON (OpenAI) or direct
/// object (Anthropic) format into a `Value`.
pub fn extract_tool_args(tool_call: &Value) -> Option<Value> {
    let args = tool_call.get("function").and_then(|f| f.get("arguments"))?;
    if let Some(s) = args.as_str() {
        serde_json::from_str::<Value>(s).ok()
    } else if args.is_object() {
        Some(args.clone())
    } else {
        None
    }
}

/// Extract the `skill_name` from a skill tool call's arguments.
///
/// Handles both stringified JSON (OpenAI: `"arguments": "{\"skill_name\":...}"`)
/// and direct JSON object (Anthropic: `"arguments": {"skill_name":...}`) formats.
pub fn extract_skill_name(tool_call: &Value) -> Option<String> {
    extract_tool_args(tool_call)?
        .get("skill_name")
        .and_then(Value::as_str)
        .map(String::from)
}

/// Execute a skill tool call from the SSE edge handler.
///
/// This is the simplified entry point for the cloud/SSE path where
/// tool calls are executed during stream consumption (before the
/// agentic loop's step 3c interception). Takes the raw `args` Value
/// from the tool call and returns the skill instructions as text.
pub async fn execute_skill_inline(
    resolver: &dyn SkillResolver,
    _tool_name: &str,
    args: &Value,
) -> String {
    let skill_name = args.get("skill_name").and_then(Value::as_str).unwrap_or("");
    let task_hint = args.get("task").and_then(Value::as_str).unwrap_or("");
    execute_skill(
        resolver,
        None,
        skill_name,
        task_hint,
        None,
        &SkillContext::default(),
    )
    .await
    .output
}

// ─── Skill execution ─────────────────────────────────────────────────────────

/// Activation effects from a skill invocation.
///
/// Returned alongside tool results so the agentic loop can apply
/// model overrides and tool restrictions to subsequent turns.
#[derive(Clone, Debug, Default)]
pub struct SkillActivation {
    /// Model override for subsequent turns (e.g. `"claude-sonnet-4-20250514"`).
    pub model_override: Option<String>,
    /// Tool allow-list — only these tools should be available.
    /// Empty means no restriction (all tools allowed).
    pub allowed_tools: Vec<String>,
    /// Effort level override for subsequent turns.
    pub effort: Option<EffortLevel>,
    /// Agent type hint (e.g. `"coder"`, `"researcher"`).
    pub agent_type: Option<String>,
    /// Sandbox policy derived from the skill's trust tier.
    /// When set, the agentic loop should apply these restrictions to tool execution.
    pub sandbox_policy: Option<crate::tool_sandbox::SandboxPolicy>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SkillVerificationOutcome {
    pub all_required_passed: bool,
    pub summary: Option<astra_services::MutationVerifierSummary>,
}

/// Result of a single skill execution.
///
/// `success` is the authoritative signal: `true` means the skill loaded and
/// ran to completion (even if its output contains warnings). `false` means the
/// skill failed to load, was blocked, or hit a validation/composition error.
/// Callers must not infer success from the `output` string content.
#[derive(Clone, Debug)]
pub struct SkillCallResult {
    pub output: String,
    pub success: bool,
    pub activation: Option<SkillActivation>,
    pub verification: Option<SkillVerificationOutcome>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InterceptedToolResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: String,
    pub verification_summary: Option<astra_services::MutationVerifierSummary>,
}

/// Handle `discover_skills` then `skill` tool calls in one batch (discover runs first).
///
/// When dynamic surfacing is off, callers typically see no `discover_skills` calls; this still
/// splits `skill` vs other tools and runs discovery first when present.
pub async fn partition_discover_and_execute_skills(
    tool_calls: &[Value],
    resolver: &dyn SkillResolver,
    catalog: &[SkillToolInfo],
    discover_exclude_lowercase: &HashSet<String>,
    discovered_skills: &mut HashSet<String>,
    executor: Option<&Arc<dyn SkillExecutor>>,
    quality_tracker: Option<&mut crate::skills::quality::SkillQualityTracker>,
    composition_ctx: Option<&crate::skills::composition::CompositionContext>,
    skill_ctx: &SkillContext,
) -> (
    Vec<InterceptedToolResult>,
    Vec<Value>,
    Option<SkillActivation>,
) {
    let mut discover_calls = Vec::new();
    let mut skill_calls = Vec::new();
    let mut other = Vec::new();
    for tc in tool_calls {
        if is_discover_skills_call(tc) {
            discover_calls.push(tc.clone());
        } else if is_skill_call(tc) {
            skill_calls.push(tc.clone());
        } else {
            other.push(tc.clone());
        }
    }

    let mut combined_results = Vec::new();
    let mut activation: Option<SkillActivation> = None;

    let mut excluded = discover_exclude_lowercase.clone();
    for n in discovered_skills.iter() {
        excluded.insert(n.to_lowercase());
    }

    for tc in discover_calls {
        let call_id = tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let args = extract_tool_args(&tc);

        let result = match args {
            Some(args) => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let (text, discovered) = execute_discover_skills(
                    query,
                    catalog,
                    excluded.clone(),
                    quality_tracker.as_deref(),
                );
                for n in &discovered {
                    discovered_skills.insert(n.clone());
                }
                for s in catalog {
                    if discovered.contains(&s.name) {
                        excluded.insert(s.name.to_lowercase());
                        for a in &s.aliases {
                            excluded.insert(a.to_lowercase());
                        }
                    }
                }
                text
            }
            None => "Invalid discover_skills arguments: expected object or JSON string".to_string(),
        };

        combined_results.push(InterceptedToolResult {
            tool_call_id: call_id,
            tool_name: tc
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            result,
            verification_summary: None,
        });
    }

    let (skill_results, _remaining_skills_only, act) = partition_and_execute_skills(
        &skill_calls,
        resolver,
        executor,
        quality_tracker,
        composition_ctx,
        skill_ctx,
    )
    .await;
    if let Some(a) = act {
        activation = Some(merge_activations(activation, a));
    }
    combined_results.extend(skill_results);

    let mut final_remaining = _remaining_skills_only;
    final_remaining.extend(other);

    (combined_results, final_remaining, activation)
}

/// Partition tool calls into skill calls and regular calls, executing skills
/// via the resolver.
///
/// When `executor` is provided, fork-context skills are executed via the executor
/// (which may run them in a sub-agent loop). Otherwise all skills are inlined.
///
/// Returns `(skill_results, remaining_tool_calls, activation)` where:
/// - `skill_results`: intercepted tool results with optional verifier summaries
/// - `remaining_tool_calls`: non-skill tool calls passed through
/// - `activation`: optional model/tool overrides from the last skill invoked
pub async fn partition_and_execute_skills(
    tool_calls: &[Value],
    resolver: &dyn SkillResolver,
    executor: Option<&Arc<dyn SkillExecutor>>,
    mut quality_tracker: Option<&mut crate::skills::quality::SkillQualityTracker>,
    composition_ctx: Option<&crate::skills::composition::CompositionContext>,
    skill_ctx: &SkillContext,
) -> (
    Vec<InterceptedToolResult>,
    Vec<Value>,
    Option<SkillActivation>,
) {
    let mut skill_results = Vec::new();
    let mut remaining = Vec::new();
    let mut activation: Option<SkillActivation> = None;

    for tc in tool_calls {
        if !is_skill_call(tc) {
            remaining.push(tc.clone());
            continue;
        }

        let call_id = tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let tool_name = tc
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let args = extract_tool_args(tc);

        let result = match args {
            Some(args) => {
                let skill_name = args.get("skill_name").and_then(Value::as_str).unwrap_or("");

                let task_hint = args.get("task").and_then(Value::as_str).unwrap_or("");

                let start = std::time::Instant::now();
                let r = execute_skill(
                    resolver,
                    executor,
                    skill_name,
                    task_hint,
                    composition_ctx,
                    skill_ctx,
                )
                .await;
                let duration_ms = start.elapsed().as_millis() as u64;

                let SkillCallResult {
                    output: skill_output,
                    success: skill_success,
                    activation: skill_activation,
                    verification: skill_verification,
                } = r;

                // Record outcome in quality tracker
                if let Some(ref mut tracker) = quality_tracker {
                    let success = skill_verification
                        .as_ref()
                        .map(|outcome| outcome.all_required_passed)
                        .unwrap_or(skill_success);
                    tracker.record_outcome(&crate::skills::quality::SkillOutcome {
                        skill_name: skill_name.to_string(),
                        tokens_used: (skill_output.len() as u32) / 4, // rough estimate
                        duration_ms,
                        all_required_passed: success,
                        partial: false,
                    });
                }

                if let Some(a) = skill_activation {
                    activation = Some(merge_activations(activation, a));
                }
                // Append a sentinel tag so the model knows the skill has fully
                // executed. The system prompt instructs the model to follow the
                // skill's instructions directly upon seeing this tag, without
                // invoking any further tools.
                // Gate on verification when present: a skill that ran but failed
                // its required checks should not be treated as successfully loaded.
                let effective_success = skill_verification
                    .as_ref()
                    .map(|v| v.all_required_passed)
                    .unwrap_or(skill_success);
                let result = if effective_success {
                    // Escape XML attribute reserved characters in skill_name.
                    let safe_name = skill_name
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('"', "&quot;")
                        .replace('\'', "&apos;");
                    format!("{}\n\n<skill-loaded name=\"{}\"/>", skill_output, safe_name)
                } else {
                    skill_output
                };
                InterceptedToolResult {
                    tool_call_id: call_id,
                    tool_name,
                    result,
                    verification_summary: skill_verification.and_then(|v| v.summary),
                }
            }
            None => InterceptedToolResult {
                tool_call_id: call_id,
                tool_name,
                result: "Invalid skill arguments: expected object or JSON string".to_string(),
                verification_summary: None,
            },
        };

        skill_results.push(result);
    }

    (skill_results, remaining, activation)
}

/// Merge a new skill activation into the accumulated activation.
///
/// When multiple skills fire in one turn, their activations must be
/// reconciled so the enforced state is consistent with ALL injected
/// instruction sets:
/// - `model_override`: None = "no opinion" (keep previous), Some = overwrite.
/// - `effort` / `agent_type`: same semantics — None preserves, Some overwrites.
/// - `allowed_tools`: intersection of all non-empty allow-lists. If any
///   skill restricts tools, only tools allowed by ALL skills survive.
///   An unrestricted skill (empty list) doesn't widen a prior restriction.
fn merge_activations(prev: Option<SkillActivation>, new: SkillActivation) -> SkillActivation {
    let Some(mut merged) = prev else {
        return new;
    };

    // Model: None = "no opinion" (keep previous), Some = overwrite.
    if new.model_override.is_some() {
        merged.model_override = new.model_override;
    }

    // Effort & agent_type: new value wins when present, otherwise keep previous.
    // None means "no opinion", not "clear".
    if new.effort.is_some() {
        merged.effort = new.effort;
    }
    if new.agent_type.is_some() {
        merged.agent_type = new.agent_type;
    }

    // Tools: intersect non-empty allow-lists.
    match (
        merged.allowed_tools.is_empty(),
        new.allowed_tools.is_empty(),
    ) {
        (true, true) => {} // Both unrestricted — stay unrestricted.
        (true, false) => {
            // Previous was unrestricted, new restricts — adopt new restrictions.
            merged.allowed_tools = new.allowed_tools;
        }
        (false, true) => {} // New is unrestricted — keep previous restrictions.
        (false, false) => {
            // Both restrict — intersect.
            let new_set: std::collections::HashSet<&str> =
                new.allowed_tools.iter().map(|s| s.as_str()).collect();
            merged
                .allowed_tools
                .retain(|t| new_set.contains(t.as_str()));
        }
    }

    // Sandbox: stricter policy wins (higher SandboxMode ordinal = more restrictive).
    match (&merged.sandbox_policy, &new.sandbox_policy) {
        (None, p @ Some(_)) => merged.sandbox_policy = p.clone(),
        (Some(prev_p), Some(new_p)) if new_p.mode > prev_p.mode => {
            merged.sandbox_policy = Some(new_p.clone());
        }
        _ => {} // prev has policy, new doesn't → keep prev; both None → nothing.
    }

    merged
}

// ─── Pipeline execution ──────────────────────────────────────────────────────

/// Execute a skill pipeline — a sequence of skills where each step's output
/// is threaded as context into the next.
///
/// Returns aggregated output + merged activation from the last step.
async fn execute_pipeline(
    resolver: &dyn SkillResolver,
    executor: Option<&Arc<dyn SkillExecutor>>,
    pipeline_skill_name: &str,
    steps: &[crate::skills::manifest::PipelineStep],
    task_hint: &str,
    composition_ctx: Option<&crate::skills::composition::CompositionContext>,
    skill_ctx: &SkillContext,
) -> SkillCallResult {
    let total = steps.len();
    let mut results: Vec<(String, String, Option<bool>)> = Vec::new();
    let mut previous_output: Option<String> = None;
    let mut last_activation: Option<SkillActivation> = None;
    let mut all_passed = true;

    for (i, step) in steps.iter().enumerate() {
        let label = step.label.as_deref().unwrap_or(step.skill.as_str());

        // Thread previous output into the task context
        let threaded_task = if let Some(ref prev) = previous_output {
            format!(
                "{}\n\n---\nPrevious step output ({}):\n{}",
                task_hint,
                results.last().map(|(l, _, _)| l.as_str()).unwrap_or(""),
                prev
            )
        } else {
            task_hint.to_string()
        };

        // Build per-step composition context with optional step timeout
        let step_ctx;
        let ctx_ref = if let Some(parent) = composition_ctx {
            step_ctx = parent.child(
                &format!("{}:{}", pipeline_skill_name, label),
                step.timeout_sec,
            );
            Some(&step_ctx)
        } else {
            None
        };

        let r = execute_skill(
            resolver,
            executor,
            &step.skill,
            &threaded_task,
            ctx_ref,
            skill_ctx,
        )
        .await;
        let SkillCallResult {
            output: step_output,
            success: step_success,
            activation: step_activation,
            verification: step_verification,
        } = r;
        let verified = step_verification
            .as_ref()
            .map(|outcome| outcome.all_required_passed);

        // Determine step success: explicit verification > success flag
        let step_passed = verified.unwrap_or(step_success);
        if !step_passed {
            all_passed = false;
        }

        if let Some(a) = step_activation {
            last_activation = Some(merge_activations(last_activation, a));
        }

        previous_output = Some(step_output.clone());
        results.push((label.to_string(), step_output, verified));

        // Stop on failure for required steps
        if step.required && !step_passed {
            let mut summary = format!(
                "# Pipeline: {}\n\n⚠️ Pipeline stopped at step {}/{}: **{}** (required step failed)\n\n",
                pipeline_skill_name,
                i + 1,
                total,
                label
            );
            for (lbl, out, passed) in &results {
                let icon = match passed {
                    Some(true) => "✅",
                    Some(false) => "❌",
                    None => "⏩",
                };
                summary.push_str(&format!("## {icon} Step: {lbl}\n\n{out}\n\n---\n\n"));
            }
            return SkillCallResult {
                output: summary,
                success: false,
                activation: last_activation,
                verification: Some(SkillVerificationOutcome {
                    all_required_passed: false,
                    summary: None,
                }),
            };
        }
    }

    // All steps completed — format aggregated output
    let mut summary = format!(
        "# Pipeline: {} — all {total} steps completed\n\n",
        pipeline_skill_name
    );
    for (lbl, out, passed) in &results {
        let icon = match passed {
            Some(true) => "✅",
            Some(false) => "⚠️",
            None => "⏩",
        };
        summary.push_str(&format!("## {icon} Step: {lbl}\n\n{out}\n\n---\n\n"));
    }

    SkillCallResult {
        output: summary,
        success: all_passed,
        activation: last_activation,
        verification: Some(SkillVerificationOutcome {
            all_required_passed: all_passed,
            summary: None,
        }),
    }
}

fn remote_response_to_text(payload: &Value) -> String {
    for key in ["result", "output", "content", "message"] {
        if let Some(value) = payload.get(key) {
            if let Some(text) = value.as_str() {
                return text.to_string();
            }
            if !value.is_null() {
                return value.to_string();
            }
        }
    }
    payload.to_string()
}

struct RemoteSkillExecutionResult {
    text: String,
    payload_json: Option<Value>,
}

const REMOTE_SKILL_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

fn remote_skill_http_client() -> &'static reqwest::Client {
    static REMOTE_SKILL_HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    REMOTE_SKILL_HTTP.get_or_init(|| {
        let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        // In test mode, skip any system proxy so localhost requests work
        // even when http_proxy/https_proxy is set without a no_proxy entry.
        if cfg!(test) {
            builder = builder.no_proxy();
        }
        builder.build().unwrap_or_else(|_| reqwest::Client::new())
    })
}

fn allow_private_remote_network() -> bool {
    if cfg!(test) {
        return true;
    }
    std::env::var("ASTRA_REMOTE_SKILL_ALLOW_PRIVATE_NET")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn remote_host_is_private_or_local(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if host.ends_with(".local") {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_multicast()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unique_local()
                    || v6.is_unicast_link_local()
                    || v6.is_multicast()
                    || v6.is_unspecified()
            }
        };
    }
    false
}

fn validate_remote_skill_endpoint(remote_url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(remote_url)
        .map_err(|err| format!("invalid remote_url '{remote_url}': {err}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported remote_url scheme '{other}'; only http/https are allowed"
            ));
        }
    }
    if remote_host_is_private_or_local(&parsed) && !allow_private_remote_network() {
        return Err(
            "remote_url resolves to localhost/private network; set ASTRA_REMOTE_SKILL_ALLOW_PRIVATE_NET=1 to explicitly allow".to_string(),
        );
    }
    Ok(())
}

fn normalize_header_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("header name cannot be empty".to_string());
    }
    let parsed = reqwest::header::HeaderName::from_bytes(trimmed.as_bytes())
        .map_err(|err| format!("invalid header name '{name}': {err}"))?;
    Ok(parsed.as_str().to_ascii_lowercase())
}

fn is_non_forwardable_header(name: &str) -> bool {
    // RFC 7230 hop-by-hop headers + transport-owned headers we never forward.
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "proxy-connection"
            | "host"
            | "content-length"
            | "content-type"
    )
}

fn connection_declared_hop_by_hop_headers(skill_ctx: &SkillContext) -> HashSet<String> {
    [CONNECTION_HEADER_TOKENS_KEY, "connection"]
        .into_iter()
        .filter_map(|name| skill_ctx.forward_headers.get(name))
        .flat_map(|raw| raw.split(','))
        .filter_map(|token| normalize_header_name(token).ok())
        .collect()
}

fn is_sensitive_remote_forward_header(name: &str) -> bool {
    matches!(
        name,
        "cookie"
            | "set-cookie"
            | "forwarded"
            | "origin"
            | "referer"
            | "x-csrf-token"
            | "x-xsrf-token"
            | "csrf-token"
            | "x-csrftoken"
            | "x-csrf"
            | "x-xsrf"
            | "x-real-ip"
            | "true-client-ip"
            | "cf-connecting-ip"
    ) || name.starts_with("sec-")
        || name.starts_with("x-forwarded-")
}

fn validate_remote_forward_header_policy(
    skill: &ResolvedSkill,
    header_kind: &str,
    name: &str,
    connection_hop_by_hop: &HashSet<String>,
) -> Result<(), String> {
    if is_non_forwardable_header(name) {
        return Err(format!(
            "skill '{}' {header_kind} '{}' is hop-by-hop/transport and cannot be forwarded",
            skill.name, name
        ));
    }
    if connection_hop_by_hop.contains(name) {
        return Err(format!(
            "skill '{}' {header_kind} '{}' is referenced by the inbound Connection header and cannot be forwarded",
            skill.name, name
        ));
    }
    if name == "authorization"
        && !matches!(
            skill.trust_tier,
            crate::skills::manifest::TrustTier::Bundled
                | crate::skills::manifest::TrustTier::Verified
        )
    {
        return Err(format!(
            "skill '{}' {header_kind} '{}' requires verified/bundled trust tier for remote forwarding",
            skill.name, name
        ));
    }
    if is_sensitive_remote_forward_header(name) {
        return Err(format!(
            "skill '{}' {header_kind} '{}' is sensitive and cannot be forwarded to remote skills",
            skill.name, name
        ));
    }
    Ok(())
}

fn resolve_remote_forward_headers(
    skill: &ResolvedSkill,
    skill_ctx: &SkillContext,
) -> Result<Vec<(String, String)>, String> {
    let connection_hop_by_hop = connection_declared_hop_by_hop_headers(skill_ctx);
    let mut required = Vec::new();
    for raw in &skill.required_headers {
        let normalized = normalize_header_name(raw).map_err(|err| {
            format!(
                "skill '{}' has invalid required_headers entry '{}': {err}",
                skill.name, raw
            )
        })?;
        validate_remote_forward_header_policy(
            skill,
            "required header",
            &normalized,
            &connection_hop_by_hop,
        )?;
        if !required.iter().any(|existing| existing == &normalized) {
            required.push(normalized);
        }
    }

    let mut requested = Vec::new();
    let mut seen = HashSet::new();
    for raw in &skill.forward_headers {
        let normalized = normalize_header_name(raw).map_err(|err| {
            format!(
                "skill '{}' has invalid forward_headers entry '{}': {err}",
                skill.name, raw
            )
        })?;
        validate_remote_forward_header_policy(
            skill,
            "forward header",
            &normalized,
            &connection_hop_by_hop,
        )?;
        if seen.insert(normalized.clone()) {
            requested.push(normalized);
        }
    }
    for normalized in &required {
        if seen.insert(normalized.clone()) {
            requested.push(normalized.clone());
        }
    }

    let missing_required: Vec<String> = required
        .into_iter()
        .filter(|name| !skill_ctx.forward_headers.contains_key(name))
        .collect();
    if !missing_required.is_empty() {
        return Err(format!(
            "missing required forwarded headers for skill '{}': {}",
            skill.name,
            missing_required.join(", ")
        ));
    }

    let forwarded = requested
        .into_iter()
        .filter_map(|name| {
            skill_ctx
                .forward_headers
                .get(&name)
                .cloned()
                .map(|value| (name, value))
        })
        .collect();
    Ok(forwarded)
}

async fn read_remote_skill_body_limited(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt;

    if let Some(content_length) = response.content_length() {
        if content_length > max_bytes as u64 {
            return Err(format!(
                "response body too large: {content_length} bytes (max {max_bytes})"
            ));
        }
    }

    let mut stream = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("failed to read response body: {err}"))?;
        let remaining = max_bytes.saturating_sub(collected.len());
        if chunk.len() > remaining {
            return Err(format!("response body exceeds max {max_bytes} bytes"));
        }
        collected.extend_from_slice(&chunk);
    }
    Ok(collected)
}

fn validate_skill_output_schema(
    skill: &ResolvedSkill,
    output_text: String,
    output_payload_json: Option<&Value>,
) -> (String, Option<SkillVerificationOutcome>) {
    let Some(schema) = skill.output_schema.as_ref() else {
        return (output_text, None);
    };
    let validation_errors = if let Some(payload_json) = output_payload_json {
        crate::skills::composition::validate_input(schema, payload_json)
    } else {
        crate::skills::composition::validate_output(schema, &output_text)
    };
    if validation_errors.is_empty() {
        return (
            output_text,
            Some(SkillVerificationOutcome {
                all_required_passed: true,
                summary: None,
            }),
        );
    }

    let mut warning = String::new();
    if !output_text.trim().is_empty() {
        warning.push_str(&output_text);
        warning.push_str("\n\n");
    }
    warning.push_str(&format!(
        "⚠️ Output schema validation failed for skill '{}':\n{}",
        skill.name,
        validation_errors
            .iter()
            .map(|err| format!("- {err}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));
    (
        warning,
        Some(SkillVerificationOutcome {
            all_required_passed: false,
            summary: None,
        }),
    )
}

async fn execute_remote_skill(
    remote_url: &str,
    skill: &ResolvedSkill,
    task_hint: &str,
    skill_ctx: &SkillContext,
) -> Result<RemoteSkillExecutionResult, String> {
    validate_remote_skill_endpoint(remote_url)?;
    let forwarded_headers = resolve_remote_forward_headers(skill, skill_ctx)?;

    // Forward a minimal context subset to reduce data-exfiltration surface for
    // user-registered remote endpoints.
    let payload = serde_json::json!({
        "skill_name": skill.name,
        "task": task_hint,
        "arguments": {
            "task": task_hint,
        },
        "context": {
            "session_id": skill_ctx.session_id,
            "recursion_depth": skill_ctx.recursion_depth,
        }
    });
    let mut request = remote_skill_http_client()
        .post(remote_url)
        .timeout(std::time::Duration::from_secs(30))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&payload);
    for (name, value) in forwarded_headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|err| format!("request failed: {err}"))?;
    let status = response.status();
    let body = read_remote_skill_body_limited(response, REMOTE_SKILL_MAX_RESPONSE_BYTES).await?;

    if !status.is_success() {
        let detail = String::from_utf8_lossy(&body).trim().to_string();
        return Err(if detail.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            format!("HTTP {}: {detail}", status.as_u16())
        });
    }

    if body.is_empty() {
        return Ok(RemoteSkillExecutionResult {
            text: String::new(),
            payload_json: None,
        });
    }
    if let Ok(json) = serde_json::from_slice::<Value>(&body) {
        return Ok(RemoteSkillExecutionResult {
            text: remote_response_to_text(&json),
            payload_json: Some(json),
        });
    }
    Ok(RemoteSkillExecutionResult {
        text: String::from_utf8_lossy(&body).to_string(),
        payload_json: None,
    })
}

/// Execute a single skill call and return the output text + activation metadata.
///
/// When the skill has `execution_context: Fork` and an executor is available,
/// the skill is run in an isolated sub-agent loop. On failure, execution falls
/// back to inline mode. MCP skills are sandboxed: inline shell commands and
/// hooks are blocked.
fn execute_skill<'a>(
    resolver: &'a dyn SkillResolver,
    executor: Option<&'a Arc<dyn SkillExecutor>>,
    skill_name: &'a str,
    task_hint: &'a str,
    composition_ctx: Option<&'a crate::skills::composition::CompositionContext>,
    skill_ctx: &'a SkillContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = SkillCallResult> + Send + 'a>> {
    Box::pin(async move {
        if let Some(ctx) = composition_ctx {
            // Depth check
            if let Err(e) = ctx.check_depth() {
                return SkillCallResult {
                    output: format!("Composition error: {e}"),
                    success: false,
                    activation: None,
                    verification: None,
                };
            }
            // Timeout check
            if let Err(e) = ctx.check_timeout() {
                return SkillCallResult {
                    output: format!("Composition error: {e}"),
                    success: false,
                    activation: None,
                    verification: None,
                };
            }
        }

        match resolver.resolve(skill_name) {
            Ok(skill) => {
                // Composability gate: nested calls must target composable skills
                if let Some(ctx) = composition_ctx {
                    if ctx.is_nested() {
                        let composable = skill
                            .composition
                            .as_ref()
                            .map(|c| c.composable)
                            .unwrap_or(false);
                        if !composable {
                            return SkillCallResult {
                                output: format!(
                                    "Composition error: skill '{}' is not composable \
                                 (set composable: true in manifest)",
                                    skill_name,
                                ),
                                success: false,
                                activation: None,
                                verification: None,
                            };
                        }
                    }
                }

                // Input schema validation
                if let Some(ref schema) = skill.input_schema {
                    let args_value: Value = serde_json::json!({ "task": task_hint });
                    let errors = crate::skills::composition::validate_input(schema, &args_value);
                    if !errors.is_empty() {
                        return SkillCallResult {
                            output: format!(
                                "Input validation failed for skill '{}':\n{}",
                                skill_name,
                                errors
                                    .iter()
                                    .map(|e| format!("  - {e}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            ),
                            success: false,
                            activation: None,
                            verification: None,
                        };
                    }
                }

                // Pipeline execution: if this skill declares steps, run them sequentially
                let has_pipeline = skill
                    .composition
                    .as_ref()
                    .map(|c| !c.steps.is_empty())
                    .unwrap_or(false);
                if has_pipeline {
                    let steps = &skill
                        .composition
                        .as_ref()
                        .expect("checked has_pipeline")
                        .steps;
                    // Create a child composition context for the pipeline
                    let pipeline_ctx;
                    let ctx_ref = match composition_ctx {
                        Some(parent) => {
                            pipeline_ctx = parent.child(
                                skill_name,
                                skill.composition.as_ref().and_then(|c| c.max_duration_sec),
                            );
                            Some(&pipeline_ctx)
                        }
                        None => {
                            let comp = skill.composition.as_ref().expect("checked has_pipeline");
                            let mut root = match comp.max_depth {
                            Some(d) => crate::skills::composition::CompositionContext::root_with_max_depth(d),
                            None => crate::skills::composition::CompositionContext::root(),
                        };
                            root.timeout_secs = comp.max_duration_sec.map(|s| s as u64);
                            pipeline_ctx = root;
                            Some(&pipeline_ctx)
                        }
                    };
                    return execute_pipeline(
                        resolver, executor, skill_name, steps, task_hint, ctx_ref, skill_ctx,
                    )
                    .await;
                }

                let is_mcp = skill.source == SkillSourceKind::Mcp;

                // MCP sandbox: block inline shell commands from untrusted sources.
                if is_mcp && crate::skills::has_inline_shell(&skill.instructions) {
                    return SkillCallResult {
                        output: format!(
                            "Skill '{}' blocked: MCP skills cannot contain inline shell commands.",
                            skill_name,
                        ),
                        success: false,
                        activation: None,
                        verification: None,
                    };
                }

                // Run pre-invocation hooks exactly once before any execution path.
                run_hooks(&skill.hooks.pre_invoke, is_mcp);

                // Remote execution: dispatch to external endpoint.
                if let Some(remote_url) = skill.remote_url.as_deref() {
                    match execute_remote_skill(remote_url, &skill, task_hint, skill_ctx).await {
                        Ok(remote_output) => {
                            let (output, verification) = validate_skill_output_schema(
                                &skill,
                                remote_output.text,
                                remote_output.payload_json.as_ref(),
                            );
                            run_hooks(&skill.hooks.post_invoke, is_mcp);
                            return SkillCallResult {
                                output,
                                success: verification
                                    .as_ref()
                                    .map(|v| v.all_required_passed)
                                    .unwrap_or(true),
                                activation: Some(build_activation(&skill)),
                                verification,
                            };
                        }
                        Err(err) => {
                            run_hooks(&skill.hooks.on_error, is_mcp);
                            return SkillCallResult {
                                output: format!("Remote skill '{}' failed: {err}", skill_name),
                                success: false,
                                activation: None,
                                verification: None,
                            };
                        }
                    }
                }

                // Fork execution: delegate to the executor for isolated sub-agent run
                if skill.execution_context == ExecutionContext::Fork {
                    if let Some(exec) = executor {
                        let instructions = substitute_arguments(
                            &skill.instructions,
                            task_hint,
                            &HashMap::new(),
                            skill.skill_dir.as_deref(),
                        );
                        let loaded = LoadedSkill {
                            manifest: SkillManifest {
                                name: skill.name.clone(),
                                model: skill.model.clone(),
                                max_tokens: skill.max_tokens,
                                allowed_tools: skill.allowed_tools.clone(),
                                execution_context: ExecutionContext::Fork,
                                hooks: Some(skill.hooks.clone()),
                                source: skill.source.clone(),
                                effort: skill.effort.clone(),
                                agent_type: skill.agent_type.clone(),
                                ..Default::default()
                            },
                            instructions,
                            instruction_tokens: (skill.instructions.len() as u32) / 4,
                            resources: None,
                            skill_dir: skill.skill_dir.as_ref().map(std::path::PathBuf::from),
                        };
                        let ctx = SkillExecutionContext {
                            task: task_hint.to_string(),
                            arguments: HashMap::new(),
                            recursion_depth: skill_ctx.recursion_depth,
                        };
                        match exec.execute(&loaded, &ctx).await {
                            Ok(result) => {
                                run_hooks(&skill.hooks.post_invoke, is_mcp);

                                // Post-execution verification (fork skills only)
                                let (output, verification) = if !skill.success_criteria.is_empty() {
                                    let work_dir = skill
                                        .skill_dir
                                        .as_ref()
                                        .map(std::path::PathBuf::from)
                                        .unwrap_or_else(|| {
                                            std::env::current_dir().unwrap_or_default()
                                        });
                                    let verifier =
                                        crate::skills::verify::SkillVerifier::new(work_dir);
                                    let mut manifest = SkillManifest::default();
                                    // Convert VerificationCriterion to serde_json::Value
                                    manifest.success_criteria = skill
                                        .success_criteria
                                        .iter()
                                        .filter_map(|c| serde_json::to_value(c).ok())
                                        .collect();
                                    let (all_passed, results) = verifier.verify(&manifest).await;

                                    let mut output = result.output;
                                    if !results.is_empty() {
                                        output.push_str("\n\n---\n**Verification Results:**\n");
                                        for r in &results {
                                            let icon = if r.passed { "✅" } else { "❌" };
                                            output.push_str(&format!(
                                                "- {} {} ({}ms){}\n",
                                                icon,
                                                r.criterion_id,
                                                r.duration_ms,
                                                if let Some(ref err) = r.error {
                                                    format!(" — {err}")
                                                } else {
                                                    String::new()
                                                }
                                            ));
                                        }
                                        if !all_passed {
                                            output.push_str(
                                            "\n⚠️ Some required verification criteria failed.\n",
                                        );
                                        }
                                    }
                                    (
                                        output,
                                        Some(SkillVerificationOutcome {
                                            all_required_passed: all_passed,
                                            summary: Some(
                                                astra_services::MutationVerifierSummary::from_results(
                                                    all_passed,
                                                    &results,
                                                ),
                                            ),
                                        }),
                                    )
                                } else {
                                    (result.output, None)
                                };

                                return SkillCallResult {
                                    output,
                                    success: verification
                                        .as_ref()
                                        .map(|v| v.all_required_passed)
                                        .unwrap_or(true),
                                    activation: Some(build_activation(&skill)),
                                    verification,
                                };
                            }
                            Err(e) => {
                                eprintln!(
                                    "  ⚠ Fork execution of skill '{}' failed: {}; falling back to inline",
                                    skill_name, e
                                );
                                // pre_invoke already ran; notify lifecycle hooks, then fall back to inline.
                                run_hooks(&skill.hooks.on_error, is_mcp);
                            }
                        }
                    }
                    // No executor available — fall through to inline
                }

                // Apply argument substitution: $ARGUMENTS, ${SKILL_DIR}, and ${CTX_*}
                let context_vars = skill_ctx.as_substitution_vars();
                let instructions = substitute_arguments(
                    &skill.instructions,
                    task_hint,
                    &context_vars,
                    skill.skill_dir.as_deref(),
                );

                // Inline execution (default, and fork-failure fallback)
                let mut output = format!(
                    "# Skill: {}\n\n\
                 You are now executing the **{}** skill. \
                 Follow the instructions below carefully.\n\n\
                 ---\n\n\
                 {}",
                    skill.name, skill.name, instructions
                );

                if !task_hint.is_empty() {
                    output.push_str(&format!("\n\n---\n\n**Task context:** {}", task_hint));
                }

                if !skill.allowed_tools.is_empty() {
                    output.push_str(&format!(
                        "\n\n**Allowed tools for this skill:** {}",
                        skill.allowed_tools.join(", ")
                    ));
                }

                // Run post-invocation hooks (skipped for MCP)
                run_hooks(&skill.hooks.post_invoke, is_mcp);

                SkillCallResult {
                    output,
                    success: true,
                    activation: Some(build_activation(&skill)),
                    verification: None,
                }
            }
            Err(e) => SkillCallResult {
                output: format!("Failed to load skill '{}': {}", skill_name, e),
                success: false,
                activation: None,
                verification: None,
            },
        }
    })
}

/// Execute hook actions synchronously. Shell commands are run with best-effort
/// (failures are logged but don't abort skill execution).
///
/// When `skip_shell` is true (MCP skills), shell hooks are silently skipped
/// to prevent untrusted skill definitions from executing arbitrary commands.
fn run_hooks(actions: &[HookAction], skip_shell: bool) {
    for action in actions {
        match action {
            HookAction::Shell { command } => {
                if skip_shell {
                    eprintln!("  ⚠ Skipping shell hook for MCP skill: {command}");
                    continue;
                }
                match std::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                {
                    Ok(out) if !out.status.success() => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        eprintln!("  ⚠ Skill hook `{command}` failed: {stderr}");
                    }
                    Err(e) => {
                        eprintln!("  ⚠ Skill hook `{command}` error: {e}");
                    }
                    _ => {}
                }
            }
            HookAction::SetEnv { key, value } => {
                if skip_shell {
                    eprintln!("  ⚠ Skipping set_env hook for MCP skill: {key}={value}");
                    continue;
                }
                // SAFETY: hook env vars are set in the current process and are
                // expected to be scoped to this skill invocation.
                unsafe { std::env::set_var(key, value) };
            }
            HookAction::Custom { id, .. } => {
                eprintln!("  ⚠ Custom hook '{id}' not yet implemented");
            }
            HookAction::Http { url, .. } => {
                eprintln!("  ⚠ HTTP hooks not supported in synchronous skill context: {url}");
            }
        }
    }
}

fn build_activation(skill: &ResolvedSkill) -> SkillActivation {
    let sandbox_policy = {
        let root = std::env::current_dir().unwrap_or_default();
        Some(crate::tool_sandbox::SandboxPolicy::for_trust_tier(
            &skill.trust_tier,
            root,
        ))
    };
    SkillActivation {
        model_override: skill.model.clone(),
        allowed_tools: skill.allowed_tools.clone(),
        effort: skill.effort.clone(),
        agent_type: skill.agent_type.clone(),
        sandbox_policy,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub resolver for tests.
    struct StubResolver {
        skills: Vec<(String, String, String)>, // (name, description, instructions)
    }

    impl SkillResolver for StubResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
            self.skills
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(n, _, inst)| ResolvedSkill {
                    name: n.clone(),
                    instructions: inst.clone(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
                .ok_or_else(|| format!("Unknown skill: {name}"))
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            self.skills
                .iter()
                .map(|(n, d, _)| SkillToolInfo {
                    name: n.clone(),
                    description: d.clone(),
                    when_to_use: None,
                    source: SkillSourceKind::Local,
                    aliases: Vec::new(),
                    category: None,
                    tags: Vec::new(),
                    triggers: Vec::new(),
                })
                .collect()
        }
    }

    fn stub_resolver() -> StubResolver {
        StubResolver {
            skills: vec![
                (
                    "code-review".into(),
                    "Review code for bugs and best practices".into(),
                    "Check for bugs, security issues, and style.".into(),
                ),
                (
                    "test-writer".into(),
                    "Generate unit tests".into(),
                    "Write comprehensive unit tests with edge cases.".into(),
                ),
            ],
        }
    }

    #[test]
    fn schema_has_correct_structure() {
        let resolver = stub_resolver();
        let skills = resolver.available_skills();
        let schema = skill_tool_schema(&skills, None, None, false);

        assert_eq!(schema["function"]["name"], SKILL_TOOL_NAME);
        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");

        let skill_enum = &params["properties"]["skill_name"]["enum"];
        assert!(skill_enum.is_array());
        let names: Vec<&str> = skill_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["code-review", "test-writer"]);
    }

    #[test]
    fn schema_open_skill_name_has_no_enum() {
        let skills = vec![SkillToolInfo {
            name: "only-one".into(),
            description: "test".into(),
            ..Default::default()
        }];
        let schema = skill_tool_schema(&skills, None, None, true);
        assert!(
            schema["function"]["parameters"]["properties"]["skill_name"]
                .get("enum")
                .is_none()
        );
    }

    #[test]
    fn discover_skills_finds_nonsurfaced_candidates() {
        let catalog = vec![
            SkillToolInfo {
                name: "surfaced".into(),
                description: "Already shown".into(),
                ..Default::default()
            },
            SkillToolInfo {
                name: "hidden-deploy".into(),
                description: "Deploy applications to Kubernetes".into(),
                ..Default::default()
            },
        ];
        let mut ex = HashSet::new();
        ex.insert("surfaced".into());
        let (text, names) = execute_discover_skills("kubernetes deploy", &catalog, ex, None);
        assert!(text.contains("hidden-deploy"), "{text}");
        assert_eq!(names, vec!["hidden-deploy".to_string()]);
    }

    #[tokio::test]
    async fn partition_runs_discover_before_skill() {
        let resolver = stub_resolver();
        let catalog = resolver.available_skills();
        let mut ex = HashSet::new();
        // Simulate "code-review" already in the turn surface; "test-writer" is discoverable.
        ex.insert("code-review".into());
        let mut discovered = HashSet::new();
        let tool_calls = vec![
            serde_json::json!({
                "id": "d1",
                "function": {
                    "name": DISCOVER_SKILLS_TOOL_NAME,
                    "arguments": "{\"query\": \"write unit tests\"}"
                }
            }),
            serde_json::json!({
                "id": "c1",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"echo hi\"}"
                }
            }),
        ];
        let (results, remaining, _) = partition_discover_and_execute_skills(
            &tool_calls,
            &resolver,
            &catalog,
            &ex,
            &mut discovered,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0].result.contains("test-writer")
                || results[0].result.contains("Additional skills")
        );
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["function"]["name"], "bash");
    }

    #[test]
    fn schema_empty_when_no_skills() {
        let schema = skill_tool_schema(&[], None, None, false);
        let skill_enum = &schema["function"]["parameters"]["properties"]["skill_name"]["enum"];
        assert_eq!(skill_enum.as_array().unwrap().len(), 0);
    }

    #[test]
    fn is_skill_call_detects_skill_tool() {
        let skill = serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "skill",
                "arguments": "{\"skill_name\": \"code-review\"}"
            }
        });
        let non_skill = serde_json::json!({
            "id": "call_2",
            "function": {
                "name": "bash",
                "arguments": "{\"command\": \"ls\"}"
            }
        });
        assert!(is_skill_call(&skill));
        assert!(!is_skill_call(&non_skill));
    }

    #[test]
    fn is_skill_call_rejects_missing_function() {
        let malformed = serde_json::json!({"id": "x"});
        assert!(!is_skill_call(&malformed));
    }

    #[test]
    fn extract_skill_name_from_stringified_args() {
        let tc = serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "skill",
                "arguments": "{\"skill_name\": \"review-changes\"}"
            }
        });
        assert_eq!(extract_skill_name(&tc), Some("review-changes".to_string()));
    }

    #[test]
    fn extract_skill_name_from_object_args() {
        let tc = serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "skill",
                "arguments": {"skill_name": "review-changes", "task": "review latest"}
            }
        });
        assert_eq!(extract_skill_name(&tc), Some("review-changes".to_string()));
    }

    #[test]
    fn extract_skill_name_returns_none_for_non_skill() {
        let tc = serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "bash",
                "arguments": "{\"command\": \"ls\"}"
            }
        });
        // Still extracts from arguments even if not a skill call
        assert_eq!(extract_skill_name(&tc), None);
    }

    #[test]
    fn extract_skill_name_returns_none_for_missing_args() {
        let tc = serde_json::json!({"id": "x"});
        assert_eq!(extract_skill_name(&tc), None);
    }

    #[test]
    fn extract_tool_args_stringified() {
        let tc = serde_json::json!({
            "function": { "name": "bash", "arguments": "{\"command\": \"ls\"}" }
        });
        let args = extract_tool_args(&tc).unwrap();
        assert_eq!(args.get("command").and_then(Value::as_str), Some("ls"));
    }

    #[test]
    fn extract_tool_args_object() {
        let tc = serde_json::json!({
            "function": { "name": "skill", "arguments": {"skill_name": "review"} }
        });
        let args = extract_tool_args(&tc).unwrap();
        assert_eq!(
            args.get("skill_name").and_then(Value::as_str),
            Some("review")
        );
    }

    #[test]
    fn extract_tool_args_missing() {
        assert!(extract_tool_args(&serde_json::json!({})).is_none());
        assert!(extract_tool_args(&serde_json::json!({"function": {}})).is_none());
    }

    #[tokio::test]
    async fn execute_skill_returns_instructions() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "code-review",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.output.contains("# Skill: code-review"));
        assert!(
            r.output
                .contains("Check for bugs, security issues, and style.")
        );
        // Activation always returned on success (even with no overrides)
        let act = r.activation.unwrap();
        assert!(act.model_override.is_none());
        assert!(act.allowed_tools.is_empty());
    }

    #[tokio::test]
    async fn execute_skill_includes_task_hint() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "code-review",
            "Review auth module",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.output.contains("**Task context:** Review auth module"));
    }

    #[tokio::test]
    async fn execute_skill_unknown_name() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "nonexistent",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.output.contains("Failed to load skill 'nonexistent'"));
        assert!(r.activation.is_none());
    }

    #[tokio::test]
    async fn execute_skill_remote_dispatches_http_endpoint() {
        use axum::{Json, Router, routing::post};

        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let app = Router::new().route(
            "/remote-skill",
            post(|Json(body): Json<Value>| async move {
                let task = body.get("task").and_then(Value::as_str).unwrap_or("");
                Json(serde_json::json!({
                    "result": format!("remote handled: {task}")
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolver {
            url: format!("http://{addr}/remote-skill"),
        };
        let r = execute_skill(
            &resolver,
            None,
            "remote-e2e",
            "ping remote",
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(r.output, "remote handled: ping remote");
        assert!(r.activation.is_some());

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_remote_forwards_configured_headers() {
        use axum::{Json, Router, http::HeaderMap, routing::post};

        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec!["authorization".into(), "x-workspace-id".into()],
                    required_headers: vec!["x-workspace-id".into()],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Verified,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let app = Router::new().route(
            "/remote-skill",
            post(|headers: HeaderMap, Json(_body): Json<Value>| async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let workspace = headers
                    .get("x-workspace-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                Json(serde_json::json!({
                    "result": format!("auth={auth};workspace={workspace}")
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolver {
            url: format!("http://{addr}/remote-skill"),
        };
        let mut skill_ctx = SkillContext::default();
        skill_ctx.forward_headers.insert(
            "authorization".to_string(),
            "Bearer trusted-token".to_string(),
        );
        skill_ctx
            .forward_headers
            .insert("x-workspace-id".to_string(), "ws-001".to_string());
        skill_ctx
            .forward_headers
            .insert("x-ignored".to_string(), "skip-me".to_string());
        let result = execute_skill(
            &resolver,
            None,
            "remote-forwarded-headers",
            "ping remote",
            None,
            &skill_ctx,
        )
        .await;
        assert_eq!(result.output, "auth=Bearer trusted-token;workspace=ws-001");
        assert!(result.activation.is_some());

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_remote_missing_required_header_fails() {
        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec!["authorization".into()],
                    required_headers: vec!["x-workspace-id".into()],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Verified,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let resolver = RemoteResolver {
            url: "https://example.com/remote-skill".to_string(),
        };
        let mut skill_ctx = SkillContext::default();
        skill_ctx.forward_headers.insert(
            "authorization".to_string(),
            "Bearer trusted-token".to_string(),
        );
        let result = execute_skill(
            &resolver,
            None,
            "remote-missing-required-header",
            "ping remote",
            None,
            &skill_ctx,
        )
        .await;
        assert!(result.output.contains("missing required forwarded headers"));
        assert!(result.output.contains("x-workspace-id"));
        assert!(result.activation.is_none());
    }

    #[test]
    fn resolve_remote_forward_headers_reports_invalid_required_header_entry() {
        let skill = ResolvedSkill {
            name: "remote-invalid-required-header".into(),
            instructions: "Remote skill placeholder.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Database,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: Some("https://example.com/remote-skill".into()),
            forward_headers: vec![],
            required_headers: vec!["bad header".into()],
            aliases: vec![],
            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Community,
        };

        let err = resolve_remote_forward_headers(&skill, &SkillContext::default())
            .expect_err("invalid required_headers entry should fail");
        assert!(err.contains("invalid required_headers entry"));
        assert!(err.contains("bad header"));
    }

    #[test]
    fn resolve_remote_forward_headers_rejects_authorization_for_community_skill() {
        let skill = ResolvedSkill {
            name: "remote-community-auth-header".into(),
            instructions: "Remote skill placeholder.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Database,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: Some("https://example.com/remote-skill".into()),
            forward_headers: vec!["authorization".into()],
            required_headers: vec![],
            aliases: vec![],
            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Community,
        };

        let mut skill_ctx = SkillContext::default();
        skill_ctx.forward_headers.insert(
            "authorization".to_string(),
            "Bearer trusted-token".to_string(),
        );

        let err = resolve_remote_forward_headers(&skill, &skill_ctx)
            .expect_err("community skill should not forward authorization");
        assert!(err.contains("requires verified/bundled trust tier"));
        assert!(err.contains("authorization"));
    }

    #[test]
    fn resolve_remote_forward_headers_rejects_cookie_header() {
        let skill = ResolvedSkill {
            name: "remote-cookie-header".into(),
            instructions: "Remote skill placeholder.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Database,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: Some("https://example.com/remote-skill".into()),
            forward_headers: vec!["cookie".into()],
            required_headers: vec![],
            aliases: vec![],
            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Verified,
        };

        let mut skill_ctx = SkillContext::default();
        skill_ctx
            .forward_headers
            .insert("cookie".to_string(), "session=secret".to_string());

        let err = resolve_remote_forward_headers(&skill, &skill_ctx)
            .expect_err("cookie should be rejected for remote forwarding");
        assert!(err.contains("sensitive"));
        assert!(err.contains("cookie"));
    }

    #[test]
    fn resolve_remote_forward_headers_rejects_dynamic_connection_token_headers() {
        let skill = ResolvedSkill {
            name: "remote-connection-token-header".into(),
            instructions: "Remote skill placeholder.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Database,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: Some("https://example.com/remote-skill".into()),
            forward_headers: vec!["x-hop".into()],
            required_headers: vec![],
            aliases: vec![],
            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Verified,
        };

        let mut skill_ctx = SkillContext::default();
        skill_ctx
            .forward_headers
            .insert("connection".to_string(), "x-hop".to_string());
        skill_ctx
            .forward_headers
            .insert("x-hop".to_string(), "secret".to_string());

        let err = resolve_remote_forward_headers(&skill, &skill_ctx)
            .expect_err("connection-declared header should not be forwardable");
        assert!(err.contains("Connection"));
        assert!(err.contains("x-hop"));
    }

    #[tokio::test]
    async fn execute_skill_remote_hop_by_hop_header_is_rejected() {
        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec!["connection".into()],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let resolver = RemoteResolver {
            url: "https://example.com/remote-skill".to_string(),
        };
        let mut skill_ctx = SkillContext::default();
        skill_ctx
            .forward_headers
            .insert("connection".to_string(), "keep-alive".to_string());
        let result = execute_skill(
            &resolver,
            None,
            "remote-hop-by-hop-header",
            "ping remote",
            None,
            &skill_ctx,
        )
        .await;
        assert!(result.output.contains("cannot be forwarded"));
        assert!(result.output.contains("connection"));
        assert!(result.activation.is_none());
    }

    #[tokio::test]
    async fn execute_skill_remote_does_not_follow_redirects() {
        use axum::{Router, response::Redirect, routing::post};

        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let app = Router::new().route(
            "/remote-skill",
            post(|| async move { Redirect::temporary("/should-not-follow") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolver {
            url: format!("http://{addr}/remote-skill"),
        };
        let result = execute_skill(
            &resolver,
            None,
            "remote-redirect",
            "ping remote",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            result.output.contains("HTTP 307"),
            "unexpected output: {}",
            result.output
        );
        assert!(result.activation.is_none());

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_remote_rejects_oversized_response_body() {
        use axum::{Router, routing::post};

        struct RemoteResolver {
            url: String,
        }
        impl SkillResolver for RemoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }

            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let oversized = "x".repeat(REMOTE_SKILL_MAX_RESPONSE_BYTES + 1);
        let app = Router::new().route(
            "/remote-skill",
            post({
                let oversized = oversized.clone();
                move || {
                    let oversized = oversized.clone();
                    async move { oversized }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolver {
            url: format!("http://{addr}/remote-skill"),
        };
        let result = execute_skill(
            &resolver,
            None,
            "remote-large-body",
            "ping remote",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            result.output.contains("response body too large")
                || result.output.contains("response body exceeds"),
            "unexpected output: {}",
            result.output
        );
        assert!(result.activation.is_none());

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_remote_output_schema_is_verified() {
        use axum::{Json, Router, routing::post};

        struct RemoteResolverWithOutputSchema {
            url: String,
        }
        impl SkillResolver for RemoteResolverWithOutputSchema {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "result": { "type": "string" }
                        },
                        "required": ["result"]
                    })),
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let app = Router::new().route(
            "/remote-skill",
            post(|Json(body): Json<Value>| async move {
                let task = body.get("task").and_then(Value::as_str).unwrap_or("");
                Json(serde_json::json!({
                    "result": format!("remote schema pass: {task}")
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolverWithOutputSchema {
            url: format!("http://{addr}/remote-skill"),
        };
        let r = execute_skill(
            &resolver,
            None,
            "remote-schema-pass",
            "ping remote",
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(r.output, "remote schema pass: ping remote");
        assert!(r.activation.is_some());
        let verification = r
            .verification
            .expect("expected output-schema verification outcome");
        assert!(verification.all_required_passed);

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_remote_output_schema_failure_marks_verification() {
        use axum::{Json, Router, routing::post};

        struct RemoteResolverWithOutputSchema {
            url: String,
        }
        impl SkillResolver for RemoteResolverWithOutputSchema {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Remote skill placeholder.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Database,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: Some(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "result": { "type": "string" }
                        },
                        "required": ["result"]
                    })),
                    remote_url: Some(self.url.clone()),
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let app = Router::new().route(
            "/remote-skill",
            post(|Json(_body): Json<Value>| async move {
                Json(serde_json::json!({
                    "status": "ok"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let resolver = RemoteResolverWithOutputSchema {
            url: format!("http://{addr}/remote-skill"),
        };
        let r = execute_skill(
            &resolver,
            None,
            "remote-schema-fail",
            "ping remote",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.output.contains("Output schema validation failed"));
        assert!(r.activation.is_some());
        let verification = r
            .verification
            .expect("expected output-schema verification outcome");
        assert!(!verification.all_required_passed);

        server.abort();
    }

    #[tokio::test]
    async fn execute_skill_fork_returns_verification_summary() {
        use async_trait::async_trait;

        struct ForkResolver {
            skill_dir: String,
        }

        impl SkillResolver for ForkResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Run forked task.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Fork,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: Some(self.skill_dir.clone()),
                    source: SkillSourceKind::Local,
                    success_criteria: vec![astra_services::VerificationCriterion {
                        id: "output-exists".into(),
                        description: "Output file exists".into(),
                        verifier: astra_services::VerifierKind::FileExists {
                            paths: vec!["output.txt".into()],
                        },
                        required: true,
                        timeout_sec: 5,
                        global_only: false,
                    }],
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        struct StubExecutor;

        #[async_trait]
        impl SkillExecutor for StubExecutor {
            async fn execute(
                &self,
                _skill: &LoadedSkill,
                _context: &SkillExecutionContext,
            ) -> Result<
                crate::skills::traits::SkillExecutionResult,
                crate::skills::traits::SkillError,
            > {
                Ok(crate::skills::traits::SkillExecutionResult {
                    output: "fork completed".into(),
                    tokens_used: 0,
                    turns: 1,
                    duration_ms: 1,
                    success: true,
                    verification_results: Vec::new(),
                    error_category: None,
                })
            }

            fn supports(&self, _context: &ExecutionContext) -> bool {
                true
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("output.txt"), "ok").unwrap();
        let resolver = ForkResolver {
            skill_dir: dir.path().to_string_lossy().into_owned(),
        };
        let executor: Arc<dyn SkillExecutor> = Arc::new(StubExecutor);

        let r = execute_skill(
            &resolver,
            Some(&executor),
            "fork-verify",
            "",
            None,
            &SkillContext::default(),
        )
        .await;

        assert!(r.output.contains("Verification Results:"));
        assert!(r.activation.is_some());
        let verification = r.verification.expect("expected verification outcome");
        assert!(verification.all_required_passed);
        let summary = verification.summary.expect("expected verifier summary");
        assert_eq!(summary.criteria_total, 1);
        assert_eq!(summary.criteria_passed, 1);
        assert!(summary.failing_criteria.is_empty());
    }

    #[test]
    fn skill_context_as_substitution_vars() {
        let ctx = SkillContext {
            session_id: Some("sess-42".into()),
            session_dir: Some("/tmp/sessions/42".into()),
            work_dir: Some("/home/user/project".into()),
            available_tools: vec!["bash".into(), "read_file".into()],
            recursion_depth: 0,
            forward_headers: HashMap::new(),
            extra: {
                let mut m = HashMap::new();
                m.insert("git_branch".into(), "main".into());
                m
            },
        };
        let vars = ctx.as_substitution_vars();
        assert_eq!(vars["CTX_SESSION_ID"], "sess-42");
        assert_eq!(vars["CTX_SESSION_DIR"], "/tmp/sessions/42");
        assert_eq!(vars["CTX_WORK_DIR"], "/home/user/project");
        assert_eq!(vars["CTX_AVAILABLE_TOOLS"], "bash, read_file");
        assert_eq!(vars["CTX_GIT_BRANCH"], "main");
        assert_eq!(vars.len(), 5);
    }

    #[test]
    fn skill_context_default_produces_empty_vars() {
        let ctx = SkillContext::default();
        assert!(ctx.as_substitution_vars().is_empty());
    }

    #[test]
    fn skill_context_debug_redacts_forward_header_values() {
        let mut ctx = SkillContext {
            session_id: Some("sess-42".into()),
            ..Default::default()
        };
        ctx.forward_headers.insert(
            "authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        ctx.forward_headers
            .insert("x-workspace-id".to_string(), "ws-123".to_string());
        ctx.forward_headers.insert(
            CONNECTION_HEADER_TOKENS_KEY.to_string(),
            "x-hop".to_string(),
        );

        let rendered = format!("{ctx:?}");
        assert!(rendered.contains("authorization"));
        assert!(rendered.contains("x-workspace-id"));
        assert!(!rendered.contains("Bearer secret-token"));
        assert!(!rendered.contains("ws-123"));
        assert!(!rendered.contains(CONNECTION_HEADER_TOKENS_KEY));
    }

    #[test]
    fn remote_host_classifier_detects_private_and_public_hosts() {
        let localhost = reqwest::Url::parse("http://127.0.0.1:8080/execute").unwrap();
        assert!(remote_host_is_private_or_local(&localhost));

        let public_ip = reqwest::Url::parse("https://8.8.8.8/execute").unwrap();
        assert!(!remote_host_is_private_or_local(&public_ip));

        let public_dns = reqwest::Url::parse("https://example.com/execute").unwrap();
        assert!(!remote_host_is_private_or_local(&public_dns));
    }

    #[tokio::test]
    async fn execute_skill_expands_context_vars() {
        // Build a resolver that includes ${CTX_WORK_DIR} in instructions
        struct CtxResolver;
        impl SkillResolver for CtxResolver {
            fn resolve(&self, _name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: "ctx-test".into(),
                    instructions: "Working in ${CTX_WORK_DIR} with session ${CTX_SESSION_ID}"
                        .into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: Default::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Bundled,
                    success_criteria: vec![],
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let ctx = SkillContext {
            work_dir: Some("/my/project".into()),
            session_id: Some("s-99".into()),
            ..Default::default()
        };
        let r = execute_skill(&CtxResolver, None, "ctx-test", "", None, &ctx).await;
        assert!(
            r.output.contains("/my/project"),
            "Expected work_dir in output, got: {}",
            r.output
        );
        assert!(
            r.output.contains("s-99"),
            "Expected session_id in output, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn partition_separates_skill_and_regular_calls() {
        let resolver = stub_resolver();
        let tool_calls = vec![
            serde_json::json!({
                "id": "call_1",
                "function": {
                    "name": "skill",
                    "arguments": "{\"skill_name\": \"code-review\"}"
                }
            }),
            serde_json::json!({
                "id": "call_2",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }),
            serde_json::json!({
                "id": "call_3",
                "function": {
                    "name": "skill",
                    "arguments": "{\"skill_name\": \"test-writer\"}"
                }
            }),
        ];

        let (skill_results, remaining, _activation) = partition_and_execute_skills(
            &tool_calls,
            &resolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;

        assert_eq!(skill_results.len(), 2);
        assert_eq!(remaining.len(), 1);

        assert_eq!(skill_results[0].tool_call_id, "call_1");
        assert_eq!(skill_results[0].tool_name, "skill");
        assert!(skill_results[0].result.contains("code-review"));
        assert!(skill_results[0].verification_summary.is_none());

        assert_eq!(skill_results[1].tool_call_id, "call_3");
        assert_eq!(skill_results[1].tool_name, "skill");
        assert!(skill_results[1].result.contains("test-writer"));
        assert!(skill_results[1].verification_summary.is_none());

        assert_eq!(remaining[0]["function"]["name"], "bash");
    }

    #[tokio::test]
    async fn partition_handles_invalid_arguments() {
        let resolver = stub_resolver();
        let tool_calls = vec![serde_json::json!({
            "id": "call_bad",
            "function": {
                "name": "skill",
                "arguments": "not valid json"
            }
        })];

        let (results, remaining, _) = partition_and_execute_skills(
            &tool_calls,
            &resolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].result.contains("Invalid skill arguments"));
        assert_eq!(remaining.len(), 0);
    }

    #[test]
    fn schema_includes_when_to_use() {
        let skills = vec![SkillToolInfo {
            name: "deployer".into(),
            description: "Deploy services".into(),
            when_to_use: Some("when user asks to deploy".into()),
            source: SkillSourceKind::Local,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
            triggers: Vec::new(),
        }];
        let schema = skill_tool_schema(&skills, None, None, false);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("when user asks to deploy"));
    }

    #[test]
    fn budget_full_descriptions_fit() {
        let skills: Vec<SkillToolInfo> = (0..5)
            .map(|i| SkillToolInfo {
                name: format!("skill-{i}"),
                description: format!("Does thing {i}"),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();
        let (entries, names) = format_skills_within_budget(&skills, 10_000, None, None);
        assert_eq!(entries.len(), 5);
        assert_eq!(names.len(), 5);
        // All entries have full descriptions
        assert!(entries[0].contains("Does thing 0"));
    }

    #[test]
    fn budget_truncates_under_pressure() {
        // Create skills that exceed a tiny budget
        let skills: Vec<SkillToolInfo> = (0..20)
            .map(|i| SkillToolInfo {
                name: format!("skill-{i}"),
                description: format!(
                    "This is a very long description for skill number {i} that goes on and on"
                ),
                when_to_use: Some(format!(
                    "when the user needs to do something very specific related to task {i}"
                )),
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();
        let (entries, names) = format_skills_within_budget(&skills, 500, None, None);
        assert_eq!(names.len(), 20); // All names still present in enum
        assert_eq!(entries.len(), 20); // All entries present
        // Entries should be shorter than full descriptions
        let total: usize = entries.iter().map(|e| e.len() + 1).sum();
        assert!(total <= 500 + 50, "total {total} should be near budget 500");
    }

    #[test]
    fn budget_bundled_preserved_others_truncated() {
        let mut skills: Vec<SkillToolInfo> = (0..3)
            .map(|i| SkillToolInfo {
                name: format!("bundled-{i}"),
                description: format!("Important bundled skill {i}"),
                when_to_use: None,
                source: SkillSourceKind::Bundled,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();
        // Add many local skills
        for i in 0..20 {
            skills.push(SkillToolInfo {
                name: format!("local-{i}"),
                description: format!("Local skill with a fairly long description for number {i}"),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            });
        }
        let (entries, names) = format_skills_within_budget(&skills, 800, None, None);
        assert_eq!(names.len(), 23);
        // Bundled entries should have full descriptions
        assert!(entries[0].contains("Important bundled skill 0"));
        assert!(entries[1].contains("Important bundled skill 1"));
        assert!(entries[2].contains("Important bundled skill 2"));
    }

    #[test]
    fn budget_names_only_under_extreme_pressure() {
        let skills: Vec<SkillToolInfo> = (0..100)
            .map(|i| SkillToolInfo {
                name: format!("s{i}"),
                description: format!("Description {i}"),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();
        // With 100 skills and 200 byte budget, names-only
        let (entries, names) = format_skills_within_budget(&skills, 200, None, None);
        assert_eq!(names.len(), 100);
        // At least some entries should be names-only (no ":")
        let names_only_count = entries.iter().filter(|e| !e.contains(": ")).count();
        assert!(
            names_only_count > 0,
            "should have names-only entries under extreme pressure"
        );
    }

    #[test]
    fn per_entry_description_capped() {
        let long_desc = "x".repeat(500);
        let skills = vec![SkillToolInfo {
            name: "long".into(),
            description: long_desc.clone(),
            when_to_use: None,
            source: SkillSourceKind::Local,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
            triggers: Vec::new(),
        }];
        let (entries, _) = format_skills_within_budget(&skills, 10_000, None, None);
        // Description should be capped at MAX_LISTING_DESC_CHARS
        assert!(
            entries[0].len() < long_desc.len(),
            "entry should be shorter than raw description"
        );
        assert!(entries[0].contains('…'), "should have truncation marker");
    }

    #[test]
    fn quality_boost_sorts_skills_under_budget_pressure() {
        use crate::skills::quality::{SkillOutcome, SkillQualityTracker};

        let skills = vec![
            SkillToolInfo {
                name: "low-quality".into(),
                description: "A skill that fails often".into(),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            },
            SkillToolInfo {
                name: "high-quality".into(),
                description: "A skill that succeeds often".into(),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            },
        ];

        let mut tracker = SkillQualityTracker::new();
        // Record 5 successes for high-quality
        for _ in 0..5 {
            tracker.record_outcome(&SkillOutcome {
                skill_name: "high-quality".into(),
                tokens_used: 100,
                duration_ms: 50,
                all_required_passed: true,
                partial: false,
            });
        }
        // Record 5 failures for low-quality
        for _ in 0..5 {
            tracker.record_outcome(&SkillOutcome {
                skill_name: "low-quality".into(),
                tokens_used: 100,
                duration_ms: 50,
                all_required_passed: false,
                partial: false,
            });
        }

        // Under budget pressure, high-quality should come first
        let (entries, _) = format_skills_within_budget(&skills, 80, Some(&tracker), None);
        // With quality sorting, high-quality should appear before low-quality
        let high_pos = entries
            .iter()
            .position(|e| e.contains("high-quality"))
            .unwrap();
        let low_pos = entries
            .iter()
            .position(|e| e.contains("low-quality"))
            .unwrap();
        assert!(
            high_pos < low_pos,
            "high-quality skill should be listed first"
        );
    }

    #[test]
    fn pinned_skills_bypass_budget_cutoff() {
        let skills: Vec<SkillToolInfo> = (0..10)
            .map(|i| SkillToolInfo {
                name: format!("skill-{i}"),
                description: format!("Description for skill {i} which is moderately long"),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();

        // Tiny budget — without pinning, many skills would be truncated
        let pinned: std::collections::HashSet<String> =
            ["skill-7".to_string()].into_iter().collect();

        let (entries, names) = format_skills_within_budget(&skills, 200, None, Some(&pinned));
        // All skill names should still be in the enum (names)
        assert!(names.contains(&"skill-7".to_string()));
        // The pinned skill should have a full description (not names-only)
        let pinned_entry = entries.iter().find(|e| e.contains("skill-7")).unwrap();
        assert!(pinned_entry.contains("Description for skill 7"));
    }

    #[test]
    fn truncate_desc_handles_cjk_without_panic() {
        // 3 bytes per CJK char — slicing at byte 5 would split a char
        let cjk = "你好世界测试";
        let result = truncate_desc(cjk, 5);
        assert!(result.ends_with('…'));
        // Should truncate to "你" (3 bytes) + "…", not panic
        assert!(result.starts_with('你'));

        // ASCII still works normally
        let ascii = "hello world";
        let result = truncate_desc(ascii, 7);
        assert_eq!(result, "hello …");
    }

    #[test]
    fn format_skill_description_truncates_cjk_safely() {
        let skill = SkillToolInfo {
            name: "cjk-skill".into(),
            description: "这是一个很长的技能描述".repeat(30), // ~330 CJK chars = ~990 bytes
            when_to_use: None,
            source: SkillSourceKind::Local,
            aliases: Vec::new(),
            category: None,
            tags: Vec::new(),
            triggers: Vec::new(),
        };
        // Should not panic even with CJK content exceeding MAX_LISTING_DESC_CHARS
        let desc = format_skill_description(&skill);
        assert!(desc.ends_with('…'));
    }

    #[test]
    fn skill_listing_includes_category_in_xml() {
        let skills = vec![
            SkillToolInfo {
                name: "code-review".into(),
                description: "Reviews code changes".into(),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: Some("review".into()),
                tags: Vec::new(),
                triggers: Vec::new(),
            },
            SkillToolInfo {
                name: "deploy".into(),
                description: "Deploys to production".into(),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None, // no category
                tags: Vec::new(),
                triggers: Vec::new(),
            },
        ];
        let msg = skill_listing_system_message(&skills, None, None, false);
        let content = msg["content"].as_str().unwrap();
        // code-review should have <category>review</category>
        assert!(content.contains("<name>code-review</name>"));
        assert!(content.contains("<category>review</category>"));
        // deploy should NOT have a category tag
        assert!(content.contains("<name>deploy</name>"));
        // But the overall content shouldn't have category for deploy
        let deploy_section = content
            .split("<skill>")
            .find(|s| s.contains("deploy"))
            .unwrap();
        assert!(!deploy_section.contains("<category>"));
    }

    #[tokio::test]
    async fn execute_skill_shows_allowed_tools() {
        struct ToolRestrictedResolver;
        impl SkillResolver for ToolRestrictedResolver {
            fn resolve(&self, _name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: "restricted".into(),
                    instructions: "Do the thing.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec!["bash".into(), "read_file".into()],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let r = execute_skill(
            &ToolRestrictedResolver,
            None,
            "restricted",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            r.output
                .contains("**Allowed tools for this skill:** bash, read_file")
        );
        // allowed_tools set → activation returned
        let act = r.activation.unwrap();
        assert_eq!(act.allowed_tools, vec!["bash", "read_file"]);
        assert!(act.model_override.is_none());
    }

    #[tokio::test]
    async fn execute_skill_returns_activation_with_model() {
        struct ModelOverrideResolver;
        impl SkillResolver for ModelOverrideResolver {
            fn resolve(&self, _name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: "fancy".into(),
                    instructions: "Be fancy.".into(),
                    model: Some("gpt-4o".into()),
                    max_tokens: Some(4096),
                    allowed_tools: vec!["bash".into()],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let r = execute_skill(
            &ModelOverrideResolver,
            None,
            "fancy",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        let act = r.activation.unwrap();
        assert_eq!(act.model_override.as_deref(), Some("gpt-4o"));
        assert_eq!(act.allowed_tools, vec!["bash"]);
    }

    #[tokio::test]
    async fn execute_skill_activation_always_returned_for_successful_resolve() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "code-review",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        // Activation is always returned on success so the loop can clear stale overrides
        let act = r.activation.unwrap();
        assert!(act.model_override.is_none());
        assert!(act.allowed_tools.is_empty());
    }

    #[tokio::test]
    async fn execute_skill_failure_returns_none_activation() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "nonexistent",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.output.contains("Failed to load skill"));
        assert!(r.activation.is_none());
    }

    #[tokio::test]
    async fn execute_skill_success_sets_success_true() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "code-review",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(r.success);
        assert!(r.activation.is_some());
    }

    #[tokio::test]
    async fn execute_skill_failure_sets_success_false() {
        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "nonexistent",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(!r.success);
        assert!(r.activation.is_none());
    }

    #[tokio::test]
    async fn partition_appends_sentinel_tag_on_success() {
        let resolver = stub_resolver();
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": { "name": "skill", "arguments": "{\"skill_name\": \"code-review\"}" }
        })];
        let (results, _, _) = partition_and_execute_skills(
            &tool_calls,
            &resolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .result
                .contains("<skill-loaded name=\"code-review\"/>"),
            "Expected sentinel tag in result, got: {}",
            results[0].result
        );
    }

    #[tokio::test]
    async fn partition_no_sentinel_tag_on_failure() {
        let resolver = stub_resolver();
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": { "name": "skill", "arguments": "{\"skill_name\": \"nonexistent\"}" }
        })];
        let (results, _, _) = partition_and_execute_skills(
            &tool_calls,
            &resolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(
            !results[0].result.contains("<skill-loaded"),
            "Sentinel tag must not appear on failure, got: {}",
            results[0].result
        );
    }

    #[tokio::test]
    async fn partition_sentinel_tag_escapes_xml_reserved_chars_in_skill_name() {
        // Skill names with XML-reserved characters must be fully escaped.
        struct QuoteResolver;
        impl SkillResolver for QuoteResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Do things.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: vec![],
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }
        // skill name contains all XML-reserved chars: & < > " '
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": { "name": "skill", "arguments": "{\"skill_name\": \"a&b<c>d\\\"e'f\"}" }
        })];
        let (results, _, _) = partition_and_execute_skills(
            &tool_calls,
            &QuoteResolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        let result = &results[0].result;
        assert!(result.contains("&amp;"), "& must be escaped, got: {result}");
        assert!(result.contains("&lt;"), "< must be escaped, got: {result}");
        assert!(result.contains("&gt;"), "> must be escaped, got: {result}");
        assert!(
            result.contains("&quot;"),
            "\" must be escaped, got: {result}"
        );
        assert!(
            result.contains("&apos;"),
            "' must be escaped, got: {result}"
        );
    }

    #[tokio::test]
    async fn execute_skill_mcp_blocked_sets_success_false() {
        struct McpResolver;
        impl SkillResolver for McpResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    // Inline shell command — blocked for MCP skills
                    instructions: "Do things.\n\n! echo hello\n".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Mcp,
                    success_criteria: vec![],
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: vec![],
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Community,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }
        let r = execute_skill(
            &McpResolver,
            None,
            "mcp-skill",
            "",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(!r.success);
        assert!(r.output.contains("blocked"));
        assert!(r.activation.is_none());
    }

    #[tokio::test]
    async fn execute_skill_composition_timeout_sets_success_false() {
        use crate::skills::composition::CompositionContext;
        let mut ctx = CompositionContext::root();
        // Set timeout to 0 so it's already expired
        ctx.timeout_secs = Some(0);
        // Sleep briefly to ensure timeout has elapsed
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;

        let resolver = stub_resolver();
        let r = execute_skill(
            &resolver,
            None,
            "code-review",
            "",
            Some(&ctx),
            &SkillContext::default(),
        )
        .await;
        assert!(!r.success);
        assert!(r.output.contains("Composition error"));
        assert!(r.activation.is_none());
    }

    #[test]
    fn build_activation_unrestricted_skill() {
        let skill = ResolvedSkill {
            name: "plain".into(),
            instructions: "Do things.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Local,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: None,
            forward_headers: vec![],
            required_headers: vec![],
            aliases: Vec::new(),

            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Bundled,
        };
        let act = super::build_activation(&skill);
        assert!(act.model_override.is_none());
        assert!(act.allowed_tools.is_empty());
    }

    #[test]
    fn build_activation_with_model_and_tools() {
        let skill = ResolvedSkill {
            name: "fancy".into(),
            instructions: "Be fancy.".into(),
            model: Some("claude-sonnet-4-20250514".into()),
            max_tokens: Some(4096),
            allowed_tools: vec!["bash".into(), "read_file".into()],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Local,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: None,
            forward_headers: vec![],
            required_headers: vec![],
            aliases: Vec::new(),

            effort: None,
            agent_type: None,
            trust_tier: crate::skills::manifest::TrustTier::Bundled,
        };
        let act = super::build_activation(&skill);
        assert_eq!(
            act.model_override.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(act.allowed_tools, vec!["bash", "read_file"]);
    }

    // ── merge_activations tests ──────────────────────────────────────────

    #[test]
    fn merge_activations_none_plus_new() {
        let new = SkillActivation {
            model_override: Some("gpt-4o".into()),
            allowed_tools: vec!["bash".into()],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(None, new);
        assert_eq!(merged.model_override.as_deref(), Some("gpt-4o"));
        assert_eq!(merged.allowed_tools, vec!["bash"]);
    }

    #[test]
    fn merge_activations_model_last_writer_wins() {
        let prev = SkillActivation {
            model_override: Some("model-a".into()),
            allowed_tools: vec![],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: Some("model-b".into()),
            allowed_tools: vec![],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert_eq!(merged.model_override.as_deref(), Some("model-b"));
    }

    #[test]
    fn merge_activations_model_none_preserves_previous() {
        let prev = SkillActivation {
            model_override: Some("model-a".into()),
            allowed_tools: vec![],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None, // no opinion — should keep "model-a"
            allowed_tools: vec![],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert_eq!(merged.model_override.as_deref(), Some("model-a"));
    }

    #[test]
    fn merge_activations_tools_intersect() {
        let prev = SkillActivation {
            model_override: None,
            allowed_tools: vec!["bash".into(), "grep".into(), "read_file".into()],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None,
            allowed_tools: vec!["bash".into(), "read_file".into(), "edit".into()],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        let mut tools = merged.allowed_tools;
        tools.sort();
        assert_eq!(tools, vec!["bash", "read_file"]);
    }

    #[test]
    fn merge_activations_unrestricted_plus_restricted() {
        let prev = SkillActivation {
            model_override: None,
            allowed_tools: vec![], // unrestricted
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None,
            allowed_tools: vec!["bash".into()], // restricted
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert_eq!(merged.allowed_tools, vec!["bash"]);
    }

    #[test]
    fn merge_activations_restricted_plus_unrestricted_keeps_restriction() {
        let prev = SkillActivation {
            model_override: None,
            allowed_tools: vec!["bash".into()], // restricted
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None,
            allowed_tools: vec![], // unrestricted
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert_eq!(merged.allowed_tools, vec!["bash"]);
    }

    #[test]
    fn merge_activations_disjoint_tools_produce_empty() {
        let prev = SkillActivation {
            model_override: None,
            allowed_tools: vec!["bash".into()],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None,
            allowed_tools: vec!["edit".into()],
            effort: None,
            agent_type: None,
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert!(merged.allowed_tools.is_empty());
    }

    #[test]
    fn build_activation_includes_effort_and_agent_type() {
        let skill = ResolvedSkill {
            name: "effort-skill".into(),
            instructions: "Work hard.".into(),
            model: None,
            max_tokens: None,
            allowed_tools: vec![],
            execution_context: ExecutionContext::Inline,
            hooks: crate::skills::hooks::SkillHooks::default(),
            skill_dir: None,
            source: SkillSourceKind::Local,
            success_criteria: Vec::new(),
            composition: None,
            input_schema: None,
            output_schema: None,
            remote_url: None,
            forward_headers: vec![],
            required_headers: vec![],
            aliases: Vec::new(),

            effort: Some(EffortLevel::High),
            agent_type: Some("coder".into()),
            trust_tier: crate::skills::manifest::TrustTier::Bundled,
        };
        let act = super::build_activation(&skill);
        assert!(matches!(act.effort, Some(EffortLevel::High)));
        assert_eq!(act.agent_type.as_deref(), Some("coder"));
        assert!(act.sandbox_policy.is_some());
    }

    #[test]
    fn merge_activations_effort_last_writer_wins() {
        let prev = SkillActivation {
            model_override: None,
            allowed_tools: vec![],
            effort: Some(EffortLevel::Low),
            agent_type: Some("researcher".into()),
            sandbox_policy: None,
        };
        let new = SkillActivation {
            model_override: None,
            allowed_tools: vec![],
            effort: Some(EffortLevel::Max),
            agent_type: None, // no opinion — should keep previous
            sandbox_policy: None,
        };
        let merged = super::merge_activations(Some(prev), new);
        assert!(matches!(merged.effort, Some(EffortLevel::Max)));
        // agent_type None = "no opinion" — previous value preserved
        assert_eq!(merged.agent_type.as_deref(), Some("researcher"));
    }

    // ── Multi-skill partition tests ──────────────────────────────────────

    #[tokio::test]
    async fn partition_multiple_skills_merges_activations() {
        struct MultiResolver;
        impl SkillResolver for MultiResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                match name {
                    "skill-a" => Ok(ResolvedSkill {
                        name: "skill-a".into(),
                        instructions: "Do A.".into(),
                        model: Some("model-a".into()),
                        max_tokens: None,
                        allowed_tools: vec!["bash".into(), "grep".into()],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: None,
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),

                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    "skill-b" => Ok(ResolvedSkill {
                        name: "skill-b".into(),
                        instructions: "Do B.".into(),
                        model: Some("model-b".into()),
                        max_tokens: None,
                        allowed_tools: vec!["bash".into(), "edit".into()],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: None,
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),

                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    _ => Err(format!("unknown: {name}")),
                }
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![
                    SkillToolInfo {
                        name: "skill-a".into(),
                        description: "A".into(),
                        when_to_use: None,
                        source: SkillSourceKind::Local,
                        aliases: Vec::new(),
                        category: None,
                        tags: Vec::new(),
                        triggers: Vec::new(),
                    },
                    SkillToolInfo {
                        name: "skill-b".into(),
                        description: "B".into(),
                        when_to_use: None,
                        source: SkillSourceKind::Local,
                        aliases: Vec::new(),
                        category: None,
                        tags: Vec::new(),
                        triggers: Vec::new(),
                    },
                ]
            }
        }

        let tool_calls = vec![
            serde_json::json!({
                "id": "c1",
                "function": { "name": "skill", "arguments": "{\"skill_name\": \"skill-a\"}" }
            }),
            serde_json::json!({
                "id": "c2",
                "function": { "name": "skill", "arguments": "{\"skill_name\": \"skill-b\"}" }
            }),
        ];

        let (results, remaining, activation) = partition_and_execute_skills(
            &tool_calls,
            &MultiResolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;

        assert_eq!(results.len(), 2);
        assert!(remaining.is_empty());

        let act = activation.unwrap();
        // Model: last writer wins → model-b
        assert_eq!(act.model_override.as_deref(), Some("model-b"));
        // Tools: intersection of [bash, grep] ∩ [bash, edit] → [bash]
        assert_eq!(act.allowed_tools, vec!["bash"]);
    }

    #[tokio::test]
    async fn partition_mixed_skill_and_failure_preserves_good_activation() {
        struct PartialResolver;
        impl SkillResolver for PartialResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                if name == "good" {
                    Ok(ResolvedSkill {
                        name: "good".into(),
                        instructions: "Do good.".into(),
                        model: Some("good-model".into()),
                        max_tokens: None,
                        allowed_tools: vec!["bash".into()],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: None,
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),

                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    })
                } else {
                    Err(format!("unknown: {name}"))
                }
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let tool_calls = vec![
            serde_json::json!({
                "id": "c1",
                "function": { "name": "skill", "arguments": "{\"skill_name\": \"good\"}" }
            }),
            serde_json::json!({
                "id": "c2",
                "function": { "name": "skill", "arguments": "{\"skill_name\": \"bad\"}" }
            }),
        ];

        let (results, _, activation) = partition_and_execute_skills(
            &tool_calls,
            &PartialResolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;

        assert_eq!(results.len(), 2);
        assert!(results[0].result.contains("# Skill: good"));
        assert!(results[1].result.contains("Failed to load skill"));

        // Good skill's activation preserved (failure returns None, doesn't overwrite)
        let act = activation.unwrap();
        assert_eq!(act.model_override.as_deref(), Some("good-model"));
        assert_eq!(act.allowed_tools, vec!["bash"]);
    }

    // ── Composition integration tests ────────────────────────────────────

    #[tokio::test]
    async fn composability_gate_blocks_non_composable_in_nested_context() {
        // Skill without composition metadata → not composable in nested context
        struct NonComposableResolver;
        impl SkillResolver for NonComposableResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Do things.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None, // not composable
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        // Nested context (depth=1)
        let parent_ctx = crate::skills::composition::CompositionContext::root();
        let child_ctx = parent_ctx.child("parent-skill", None);

        let r = execute_skill(
            &NonComposableResolver,
            None,
            "child-skill",
            "do work",
            Some(&child_ctx),
            &SkillContext::default(),
        )
        .await;
        assert!(
            r.output.contains("not composable"),
            "Expected composability error, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn composable_skill_allowed_in_nested_context() {
        struct ComposableResolver;
        impl SkillResolver for ComposableResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Do composable things.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: Some(crate::skills::manifest::SkillComposition {
                        composable: true,
                        idempotent: false,
                        side_effects: vec![],
                        max_duration_sec: None,
                        max_depth: None,
                        steps: vec![],
                    }),
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let parent_ctx = crate::skills::composition::CompositionContext::root();
        let child_ctx = parent_ctx.child("parent-skill", None);

        let r = execute_skill(
            &ComposableResolver,
            None,
            "child-skill",
            "do work",
            Some(&child_ctx),
            &SkillContext::default(),
        )
        .await;
        // Should succeed (inline injection)
        assert!(
            r.output.contains("Do composable things"),
            "Expected skill output, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn depth_limit_blocks_deeply_nested() {
        struct AnyResolver;
        impl SkillResolver for AnyResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Deep skill.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: Some(crate::skills::manifest::SkillComposition {
                        composable: true,
                        idempotent: false,
                        side_effects: vec![],
                        max_duration_sec: None,
                        max_depth: None,
                        steps: vec![],
                    }),
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        // Build a context at max depth
        let mut ctx = crate::skills::composition::CompositionContext::root();
        for i in 0..crate::skills::composition::MAX_COMPOSITION_DEPTH {
            ctx = ctx.child(&format!("level-{i}"), None);
        }

        let r = execute_skill(
            &AnyResolver,
            None,
            "too-deep",
            "work",
            Some(&ctx),
            &SkillContext::default(),
        )
        .await;
        assert!(
            r.output.contains("depth"),
            "Expected depth error, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn top_level_call_skips_composability_check() {
        // Non-composable skill should work fine at top level (depth=0)
        struct NonComposableResolver;
        impl SkillResolver for NonComposableResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Top level only.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        // Root context (depth=0) — composability check should not apply
        let root_ctx = crate::skills::composition::CompositionContext::root();
        let r = execute_skill(
            &NonComposableResolver,
            None,
            "my-skill",
            "work",
            Some(&root_ctx),
            &SkillContext::default(),
        )
        .await;
        assert!(
            !r.output.contains("not composable"),
            "Root call should not check composability"
        );
        assert!(
            r.output.contains("Top level only"),
            "Expected skill output, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn input_schema_validation_blocks_invalid_args() {
        struct SchemaResolver;
        impl SkillResolver for SchemaResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "Schema skill.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: None,
                    input_schema: Some(serde_json::json!({
                        "properties": {
                            "target_path": { "type": "string" }
                        },
                        "required": ["target_path"]
                    })),
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),

                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        // The execute_skill builds args as {"task": "..."}, which won't have "target_path"
        let r = execute_skill(
            &SchemaResolver,
            None,
            "schema-skill",
            "do stuff",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            r.output.contains("validation failed"),
            "Expected validation error, got: {}",
            r.output
        );
    }

    #[test]
    fn pinned_skill_gets_full_description_even_with_quality_sorting() {
        use crate::skills::quality::SkillQualityTracker;

        let mut tracker = SkillQualityTracker::new();
        // Record high-quality outcomes for skill-0 to give it high boost
        for _ in 0..5 {
            tracker.record_outcome(&crate::skills::quality::SkillOutcome {
                skill_name: "skill-0".into(),
                tokens_used: 100,
                duration_ms: 50,
                all_required_passed: true,
                partial: false,
            });
        }
        // skill-9 (pinned) has no quality data — would normally be low priority

        let skills: Vec<SkillToolInfo> = (0..10)
            .map(|i| SkillToolInfo {
                name: format!("skill-{i}"),
                description: format!("Description for skill {i} which is moderately long text"),
                when_to_use: None,
                source: SkillSourceKind::Local,
                aliases: Vec::new(),
                category: None,
                tags: Vec::new(),
                triggers: Vec::new(),
            })
            .collect();

        let pinned: std::collections::HashSet<String> =
            ["skill-9".to_string()].into_iter().collect();

        // Very tight budget — forces truncation
        let (entries, names) =
            format_skills_within_budget(&skills, 250, Some(&tracker), Some(&pinned));

        // Pinned skill-9 must have full description (treated as bundled)
        let pinned_entry = entries.iter().find(|e| e.contains("skill-9")).unwrap();
        assert!(
            pinned_entry.contains("Description for skill 9"),
            "Pinned skill should have full description, got: {pinned_entry}"
        );

        // All names still in enum
        assert!(names.contains(&"skill-9".to_string()));
        assert!(names.contains(&"skill-0".to_string()));
    }

    // ─── Pipeline execution tests ────────────────────────────────────────────

    /// Resolver that supports pipeline execution: a "pipeline-skill" with two steps
    /// that resolve to "step-a" and "step-b".
    struct PipelineResolver;
    impl SkillResolver for PipelineResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
            match name {
                "pipeline-skill" => Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: "This is a pipeline skill.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: Some(crate::skills::manifest::SkillComposition {
                        composable: false,
                        idempotent: false,
                        side_effects: vec![],
                        max_duration_sec: Some(300),
                        max_depth: None,
                        steps: vec![
                            crate::skills::manifest::PipelineStep {
                                skill: "step-a".into(),
                                label: Some("Analyze".into()),
                                timeout_sec: None,
                                required: true,
                            },
                            crate::skills::manifest::PipelineStep {
                                skill: "step-b".into(),
                                label: Some("Build".into()),
                                timeout_sec: None,
                                required: true,
                            },
                        ],
                    }),
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                }),
                "step-a" | "step-b" => Ok(ResolvedSkill {
                    name: name.into(),
                    instructions: format!("Instructions for {name}."),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                    execution_context: ExecutionContext::Inline,
                    hooks: crate::skills::hooks::SkillHooks::default(),
                    skill_dir: None,
                    source: SkillSourceKind::Local,
                    success_criteria: Vec::new(),
                    composition: Some(crate::skills::manifest::SkillComposition {
                        composable: true,
                        idempotent: true,
                        side_effects: vec![],
                        max_duration_sec: None,
                        max_depth: None,
                        steps: vec![],
                    }),
                    input_schema: None,
                    output_schema: None,
                    remote_url: None,
                    forward_headers: vec![],
                    required_headers: vec![],
                    aliases: Vec::new(),
                    effort: None,
                    agent_type: None,
                    trust_tier: crate::skills::manifest::TrustTier::Bundled,
                }),
                _ => Err(format!("Unknown skill: {name}")),
            }
        }
        fn available_skills(&self) -> Vec<SkillToolInfo> {
            vec![]
        }
    }

    #[tokio::test]
    async fn pipeline_executes_all_steps_sequentially() {
        let resolver = PipelineResolver;
        let r = execute_skill(
            &resolver,
            None,
            "pipeline-skill",
            "run it",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            r.output.contains("all 2 steps completed"),
            "Expected pipeline completion, got: {}",
            r.output
        );
        assert!(
            r.output.contains("Step: Analyze"),
            "Should contain step-a label"
        );
        assert!(
            r.output.contains("Step: Build"),
            "Should contain step-b label"
        );
        assert!(
            r.output.contains("Instructions for step-a"),
            "Should contain step-a output"
        );
        assert!(
            r.output.contains("Instructions for step-b"),
            "Should contain step-b output"
        );
        assert!(r.activation.is_some());
        assert_eq!(
            r.verification
                .as_ref()
                .map(|outcome| outcome.all_required_passed),
            Some(true)
        );
    }

    #[tokio::test]
    async fn pipeline_threads_output_between_steps() {
        let resolver = PipelineResolver;
        let r = execute_skill(
            &resolver,
            None,
            "pipeline-skill",
            "check threading",
            None,
            &SkillContext::default(),
        )
        .await;
        // Step B should receive step A's output threaded in
        assert!(
            r.output.contains("Previous step output"),
            "Expected output threading between steps"
        );
    }

    #[tokio::test]
    async fn pipeline_stops_on_required_step_failure() {
        struct FailingPipelineResolver;
        impl SkillResolver for FailingPipelineResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                match name {
                    "fail-pipeline" => Ok(ResolvedSkill {
                        name: name.into(),
                        instructions: "Pipeline.".into(),
                        model: None,
                        max_tokens: None,
                        allowed_tools: vec![],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: Some(crate::skills::manifest::SkillComposition {
                            composable: false,
                            idempotent: false,
                            side_effects: vec![],
                            max_duration_sec: None,
                            max_depth: None,
                            steps: vec![
                                crate::skills::manifest::PipelineStep {
                                    skill: "ok-step".into(),
                                    label: None,
                                    timeout_sec: None,
                                    required: true,
                                },
                                crate::skills::manifest::PipelineStep {
                                    skill: "missing-step".into(),
                                    label: None,
                                    timeout_sec: None,
                                    required: true,
                                },
                                crate::skills::manifest::PipelineStep {
                                    skill: "never-reached".into(),
                                    label: None,
                                    timeout_sec: None,
                                    required: true,
                                },
                            ],
                        }),
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),
                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    "ok-step" => Ok(ResolvedSkill {
                        name: name.into(),
                        instructions: "OK step.".into(),
                        model: None,
                        max_tokens: None,
                        allowed_tools: vec![],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: Vec::new(),
                        composition: Some(crate::skills::manifest::SkillComposition {
                            composable: true,
                            idempotent: true,
                            side_effects: vec![],
                            max_duration_sec: None,
                            max_depth: None,
                            steps: vec![],
                        }),
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: Vec::new(),
                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    _ => Err(format!("Unknown skill: {name}")),
                }
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let resolver = FailingPipelineResolver;
        let r = execute_skill(
            &resolver,
            None,
            "fail-pipeline",
            "test",
            None,
            &SkillContext::default(),
        )
        .await;
        // The missing-step should fail resolution, and never-reached should not appear
        assert!(
            !r.output.contains("never-reached"),
            "Pipeline should stop before 3rd step"
        );
        assert!(
            r.output.contains("Failed to load skill"),
            "Should show load failure"
        );
    }

    #[tokio::test]
    async fn pipeline_failure_sets_success_false() {
        // Run an actual pipeline skill with a required step that fails to resolve.
        // execute_pipeline must return success=false and no sentinel tag.
        struct FailPipelineResolver2;
        impl SkillResolver for FailPipelineResolver2 {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                match name {
                    "fail-pipeline2" => Ok(ResolvedSkill {
                        name: name.into(),
                        instructions: "Pipeline.".into(),
                        model: None,
                        max_tokens: None,
                        allowed_tools: vec![],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: vec![],
                        composition: Some(crate::skills::manifest::SkillComposition {
                            composable: false,
                            idempotent: false,
                            side_effects: vec![],
                            max_duration_sec: None,
                            max_depth: None,
                            steps: vec![crate::skills::manifest::PipelineStep {
                                skill: "missing-step".into(),
                                label: None,
                                timeout_sec: None,
                                required: true,
                            }],
                        }),
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: vec![],
                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    _ => Err(format!("Unknown skill: {name}")),
                }
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }
        let r = execute_skill(
            &FailPipelineResolver2,
            None,
            "fail-pipeline2",
            "test",
            None,
            &SkillContext::default(),
        )
        .await;
        assert!(
            !r.success,
            "Pipeline with required failing step must have success=false"
        );
        assert!(
            !r.output.contains("<skill-loaded"),
            "Sentinel tag must not appear for failed pipeline, got: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn pipeline_success_sets_success_true_and_appends_sentinel() {
        use crate::skills::manifest::{PipelineStep, SkillComposition};
        struct TwoStepResolver;
        impl SkillResolver for TwoStepResolver {
            fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
                match name {
                    "pipeline-two" => Ok(ResolvedSkill {
                        name: name.into(),
                        instructions: "Pipeline.".into(),
                        model: None,
                        max_tokens: None,
                        allowed_tools: vec![],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: vec![],
                        composition: Some(SkillComposition {
                            composable: false,
                            idempotent: false,
                            side_effects: vec![],
                            max_duration_sec: None,
                            max_depth: None,
                            steps: vec![
                                PipelineStep {
                                    skill: "step-a".into(),
                                    label: Some("A".into()),
                                    required: true,
                                    timeout_sec: None,
                                },
                                PipelineStep {
                                    skill: "step-b".into(),
                                    label: Some("B".into()),
                                    required: true,
                                    timeout_sec: None,
                                },
                            ],
                        }),
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: vec![],
                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    "step-a" | "step-b" => Ok(ResolvedSkill {
                        name: name.into(),
                        instructions: format!("Instructions for {name}."),
                        model: None,
                        max_tokens: None,
                        allowed_tools: vec![],
                        execution_context: ExecutionContext::Inline,
                        hooks: crate::skills::hooks::SkillHooks::default(),
                        skill_dir: None,
                        source: SkillSourceKind::Local,
                        success_criteria: vec![],
                        composition: Some(SkillComposition {
                            composable: true,
                            idempotent: false,
                            side_effects: vec![],
                            max_duration_sec: None,
                            max_depth: None,
                            steps: vec![],
                        }),
                        input_schema: None,
                        output_schema: None,
                        remote_url: None,
                        forward_headers: vec![],
                        required_headers: vec![],
                        aliases: vec![],
                        effort: None,
                        agent_type: None,
                        trust_tier: crate::skills::manifest::TrustTier::Bundled,
                    }),
                    _ => Err(format!("Unknown skill: {name}")),
                }
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }
        let tool_calls = vec![serde_json::json!({
            "id": "call_1",
            "function": { "name": "skill", "arguments": "{\"skill_name\": \"pipeline-two\"}" }
        })];
        let (results, _, _) = partition_and_execute_skills(
            &tool_calls,
            &TwoStepResolver,
            None,
            None,
            None,
            &SkillContext::default(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(
            results[0]
                .result
                .contains("<skill-loaded name=\"pipeline-two\"/>"),
            "Successful pipeline must have sentinel tag, got: {}",
            results[0].result
        );
        assert!(
            results[0].result.contains("all 2 steps completed"),
            "Expected completion message, got: {}",
            results[0].result
        );
    }

    #[tokio::test]
    async fn pipeline_steps_parsed_from_yaml() {
        let skill_md = r#"---
name: my-pipeline
description: "Test pipeline"
composition:
  steps:
    - skill: analyze
      label: "Step 1"
      timeout_sec: 60
    - skill: fix
      required: false
---
Run the pipeline.
"#;
        let (manifest, _body) = crate::skills::loader::parse_skill_md(skill_md).unwrap();
        let comp = manifest.composition.unwrap();
        assert_eq!(comp.steps.len(), 2);
        assert_eq!(comp.steps[0].skill, "analyze");
        assert_eq!(comp.steps[0].label.as_deref(), Some("Step 1"));
        assert_eq!(comp.steps[0].timeout_sec, Some(60));
        assert!(comp.steps[0].required); // default true
        assert_eq!(comp.steps[1].skill, "fix");
        assert!(!comp.steps[1].required);
    }

    #[tokio::test]
    async fn max_depth_parsed_from_yaml() {
        let skill_md = r#"---
name: deep-skill
description: "Skill with custom depth"
composition:
  composable: true
  max_depth: 5
---
Deep nesting allowed.
"#;
        let (manifest, _body) = crate::skills::loader::parse_skill_md(skill_md).unwrap();
        let comp = manifest.composition.unwrap();
        assert_eq!(comp.max_depth, Some(5));
    }

    #[tokio::test]
    async fn max_depth_defaults_to_none() {
        let skill_md = r#"---
name: normal-skill
description: "No custom depth"
composition:
  composable: true
---
Normal.
"#;
        let (manifest, _body) = crate::skills::loader::parse_skill_md(skill_md).unwrap();
        let comp = manifest.composition.unwrap();
        assert_eq!(comp.max_depth, None);
    }
}
