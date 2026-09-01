//! YAML case loader.
//!
//! Each case is a single YAML document describing one behavior we
//! want to verify, independent of the model that implements it.
//! Cases are runnable across a model matrix — the same prompt is
//! replayed against each model in `models:` (or the CLI-provided
//! fallback list).
//!
//! See `cases/hello_text_contains.yaml` for a worked example.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single test case. One YAML file == one `Case`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Human-readable id. Appears in report lines. Unique per suite.
    pub name: String,

    /// Optional longer description — surfaced in reports with
    /// `--verbose`. Keep short; expectations go in `criteria`.
    #[serde(default)]
    pub description: Option<String>,

    /// Prompt sent to the model as the first user message. Mirrors
    /// what a developer would paste into `astra chat -m "..."`.
    pub prompt: String,

    /// Meaning-preserving rewrites of one user turn. They are dormant unless
    /// the runner enables prompt-variant expansion, then each rewrite is
    /// evaluated with the exact same typed criteria as the canonical journey.
    /// This is a metamorphic test: it detects prompt-shape overfitting without
    /// requiring byte-identical model answers.
    #[serde(default, skip_serializing)]
    pub prompt_variants: Vec<PromptVariant>,

    /// Optional model list for this case. When omitted, the CLI
    /// `--models` flag provides the fallback list. When BOTH are
    /// omitted, the runner errors.
    #[serde(default)]
    pub models: Option<Vec<String>>,

    /// Success criteria evaluated in order. All must pass for the
    /// case to PASS. Order matters — deterministic checks first,
    /// expensive LLM judger last.
    #[serde(default)]
    pub criteria: Vec<super::criteria::Criterion>,

    /// When true, the runner captures stderr verbatim in the report
    /// (sink output, fork-capture logs, tool invocation lines).
    /// Default false — reports compress to pass/fail counts.
    #[serde(default)]
    pub debug_log: bool,

    /// Optional extra CLI flags passed through to `astra chat`.
    /// Intended escape hatch for cases that need e.g. `--explain`.
    /// Flags are appended after harness-managed flags — and most
    /// CLI parsers let later flags win, so a malicious or careless
    /// case could clobber `-m/--model/--json/-y/--message`. The
    /// loader rejects any reserved flag at parse time; this comment
    /// is documentation only, the enforcement is in
    /// [`validate_extra_cli_args`].
    #[serde(default)]
    pub extra_cli_args: Vec<String>,

    /// Optional per-case timeout in seconds. Defaults to 180.
    /// Caps runaway models without silently skewing matrix totals.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,

    /// Capability dimension this case tests. Used for aggregated
    /// reporting by capability × model.
    #[serde(default)]
    pub capability: Option<Capability>,

    /// Optional prompt-cache reuse scope this case requires from the
    /// target model. A model profile that explicitly cannot satisfy the
    /// scope yields an `unavailable` run; it is never skip-passed. Unknown
    /// metadata leaves the case runnable so its criteria can provide real
    /// evidence.
    #[serde(default)]
    pub required_cache_scope: Option<PromptCacheReuseScope>,

    /// Difficulty level 1–5. Higher = harder. Used for weighted scoring.
    #[serde(default)]
    pub difficulty: Option<u8>,

    /// Scoring weight (default 1.0). Cases with higher weight
    /// contribute more to the aggregate pass rate.
    #[serde(default = "default_weight")]
    pub weight: f64,

    /// Multi-turn steps. When present, `prompt` is the first step
    /// and `steps` contains follow-up turns. Each step has its own
    /// prompt and optional criteria.
    #[serde(default)]
    pub steps: Vec<CaseStep>,

    /// Environment variables injected into the `astra` CLI subprocess only.
    ///
    /// These values do not configure the remote Server. Server-owned behavior
    /// (for example compaction policy) must be configured on the Server that
    /// the harness profile targets and proved through durable Server evidence.
    #[serde(default)]
    pub cli_env: std::collections::HashMap<String, String>,

    /// Shell command to run before the case (e.g., create temp files).
    /// Runs in `working_dir` or CWD. Non-zero exit aborts the case.
    #[serde(default)]
    pub setup_cmd: Option<String>,

    /// Shell command to run after the case (cleanup). Always runs,
    /// even on failure.
    #[serde(default)]
    pub teardown_cmd: Option<String>,

    /// Remove only memory records that this case demonstrably created.
    ///
    /// The harness derives exact IDs from the case session's structured
    /// `ToolCallCompleted` events, then invokes the normal authenticated
    /// `astra memory forget` command. This deliberately avoids broad topic
    /// purges and avoids a separate privileged Memoria credential.
    #[serde(default)]
    pub cleanup_memory_records: bool,
}

/// A follow-up turn in a multi-turn case.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseStep {
    /// Prompt for this turn.
    pub prompt: String,
    /// Per-step criteria evaluated against this step's outcome.
    /// If any step criterion fails, the overall case FAILs.
    #[serde(default)]
    pub criteria: Vec<super::criteria::Criterion>,
    /// Optional per-step timeout override.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// One meaning-preserving rewrite of a user turn in a case journey.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptVariant {
    /// Stable report suffix (`case@id`). Lowercase ASCII keeps artifact paths
    /// and filters portable across platforms.
    pub id: String,
    /// Follow-up index in `Case.steps`. Omit to target the initial prompt.
    /// Keeping the target explicit lets long-session referent handling use the
    /// same metamorphic contract without cloning a scenario-specific case.
    #[serde(default)]
    pub step_index: Option<usize>,
    /// Complete replacement for the selected user prompt.
    pub prompt: String,
}

/// Capability dimension for aggregated reporting.
///
/// Known variants use snake_case. Custom capabilities MUST use the
/// `"custom:my_name"` prefix — bare unknown strings are rejected at
/// load time so typos don't silently pass.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    ToolUse,
    Delegation,
    InstructionFollowing,
    AntiHallucination,
    Efficiency,
    CodeGeneration,
    Reasoning,
    Memory,
    Planning,
    /// Custom capability — YAML must use `"custom:my_name"` syntax.
    Custom(String),
}

pub type PromptCacheReuseScope = astra_services::PromptCacheReuseScopeData;

/// Canonicalize the execution matrix at its trust boundary. Model IDs are
/// opaque and case-sensitive; whitespace is never part of an ID, and a
/// duplicate would create two indistinguishable report rows with the same
/// run_index.
pub(crate) fn canonicalize_model_ids(models: &[String]) -> Result<Vec<String>, String> {
    let mut canonical = Vec::with_capacity(models.len());
    for raw in models {
        let model = raw.trim();
        if model.is_empty() {
            return Err("model matrix contains an empty or whitespace-only ID".into());
        }
        if model != raw {
            return Err(format!(
                "model matrix ID {raw:?} has surrounding whitespace; IDs must be canonical"
            ));
        }
        if canonical.iter().any(|seen| seen == model) {
            return Err(format!("model matrix contains duplicate ID {model:?}"));
        }
        canonical.push(model.to_string());
    }
    if canonical.is_empty() {
        return Err("model matrix must contain at least one model ID".into());
    }
    Ok(canonical)
}

impl<'de> serde::Deserialize<'de> for Capability {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "tool_use" => Ok(Self::ToolUse),
            "delegation" => Ok(Self::Delegation),
            "instruction_following" => Ok(Self::InstructionFollowing),
            "anti_hallucination" => Ok(Self::AntiHallucination),
            "efficiency" => Ok(Self::Efficiency),
            "code_generation" => Ok(Self::CodeGeneration),
            "reasoning" => Ok(Self::Reasoning),
            "memory" => Ok(Self::Memory),
            "planning" => Ok(Self::Planning),
            other if other.starts_with("custom:") => {
                let name = other.strip_prefix("custom:").unwrap().to_string();
                if name.is_empty() {
                    return Err(serde::de::Error::custom(
                        "capability 'custom:' requires a name after the colon",
                    ));
                }
                Ok(Self::Custom(name))
            }
            other => Err(serde::de::Error::custom(format!(
                "unknown capability {other:?}. Known: {}. \
                 For custom capabilities use \"custom:my_name\" syntax.",
                KNOWN_CAPABILITIES.join(", ")
            ))),
        }
    }
}

impl serde::Serialize for Capability {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolUse => write!(f, "tool_use"),
            Self::Delegation => write!(f, "delegation"),
            Self::InstructionFollowing => write!(f, "instruction_following"),
            Self::AntiHallucination => write!(f, "anti_hallucination"),
            Self::Efficiency => write!(f, "efficiency"),
            Self::CodeGeneration => write!(f, "code_generation"),
            Self::Reasoning => write!(f, "reasoning"),
            Self::Memory => write!(f, "memory"),
            Self::Planning => write!(f, "planning"),
            Self::Custom(s) => write!(f, "custom:{s}"),
        }
    }
}

const KNOWN_CAPABILITIES: &[&str] = &[
    "tool_use",
    "delegation",
    "instruction_following",
    "anti_hallucination",
    "efficiency",
    "code_generation",
    "reasoning",
    "memory",
    "planning",
];

fn default_weight() -> f64 {
    1.0
}

fn default_timeout_seconds() -> u64 {
    180
}

const MAX_PROMPT_VARIANTS_PER_CASE: usize = 8;

fn validate_prompt_variants(case: &Case) -> Result<(), String> {
    if case.prompt_variants.len() > MAX_PROMPT_VARIANTS_PER_CASE {
        return Err(format!(
            "prompt_variants has {} entries; maximum is {MAX_PROMPT_VARIANTS_PER_CASE}",
            case.prompt_variants.len()
        ));
    }
    let id_pattern = regex::Regex::new(r"^[a-z0-9][a-z0-9_-]{0,63}$")
        .expect("prompt variant id regex is static");
    let mut ids = std::collections::BTreeSet::new();
    let mut rewrites = std::collections::BTreeSet::new();
    for (index, variant) in case.prompt_variants.iter().enumerate() {
        if !id_pattern.is_match(&variant.id) {
            return Err(format!(
                "prompt_variants[{index}].id {:?} must match [a-z0-9][a-z0-9_-]{{0,63}}",
                variant.id
            ));
        }
        if !ids.insert(variant.id.as_str()) {
            return Err(format!(
                "prompt_variants contains duplicate id {:?}",
                variant.id
            ));
        }
        let prompt = variant.prompt.trim();
        if prompt.is_empty() {
            return Err(format!("prompt_variants[{index}].prompt must not be empty"));
        }
        let canonical = match variant.step_index {
            None => case.prompt.trim(),
            Some(step_index) => case
                .steps
                .get(step_index)
                .ok_or_else(|| {
                    format!(
                        "prompt_variants[{index}].step_index {step_index} is out of range for {} follow-up step(s)",
                        case.steps.len()
                    )
                })?
                .prompt
                .trim(),
        };
        if prompt == canonical {
            return Err(format!(
                "prompt_variants[{index}].prompt duplicates its targeted canonical prompt"
            ));
        }
        if !rewrites.insert((variant.step_index, prompt.to_string())) {
            return Err(format!(
                "prompt_variants[{index}] duplicates another rewrite for the same turn"
            ));
        }
    }
    Ok(())
}

/// Expand dormant prompt variants into ordinary cases with shared criteria.
/// The original case weight is divided across the equivalence class so
/// enabling robustness checks cannot silently increase that capability's
/// aggregate score. Returns the number of added executions.
pub fn expand_prompt_variants(cases: &mut Vec<Case>) -> Result<usize, String> {
    let mut expanded = Vec::new();
    let mut names = std::collections::BTreeSet::new();
    let mut added = 0usize;
    for mut canonical in cases.clone() {
        validate_prompt_variants(&canonical)
            .map_err(|error| format!("case {:?}: {error}", canonical.name))?;
        let variants = std::mem::take(&mut canonical.prompt_variants);
        let shared_weight = canonical.weight / (variants.len() + 1) as f64;
        canonical.weight = shared_weight;
        if !names.insert(canonical.name.clone()) {
            return Err(format!("duplicate expanded case name {:?}", canonical.name));
        }
        expanded.push(canonical.clone());
        for variant in variants {
            let mut case = canonical.clone();
            case.name = format!("{}@{}", canonical.name, variant.id);
            match variant.step_index {
                None => case.prompt = variant.prompt,
                Some(step_index) => case.steps[step_index].prompt = variant.prompt,
            }
            if !names.insert(case.name.clone()) {
                return Err(format!("duplicate expanded case name {:?}", case.name));
            }
            expanded.push(case);
            added += 1;
        }
    }
    expanded.sort_by(|left, right| left.name.cmp(&right.name));
    *cases = expanded;
    Ok(added)
}

/// Flags the harness owns and must not be overridden by a case.
/// The CLI parser chosen by `astra chat` lets later args win for most
/// of these, so a case that included them could silently clobber the
/// harness's prompt, output format, or safety boundaries — making the
/// whole run meaningless (or worse, unsafe). Fail fast at case load.
///
/// Entries cover both the real `astra chat` subcommand flag names and
/// top-level astra aliases (`-y`/`--yes` top-level, `--auto-approve`
/// on `chat`). `--approve-all` from the original denylist was
/// removed — that flag does not exist in the CLI surface.
pub(crate) const RESERVED_CLI_ARGS: &[&str] = &[
    // Prompt / message input — the harness owns the prompt.
    "-m",
    "--message",
    "--stdin",
    // Model selection — harness controls via --models / case.models.
    "--model",
    // Output format — harness parses --json.
    "--json",
    "--quiet",
    // Tool approval — the harness auto-approves (`-y`).
    "-y",
    "--yes",
    "--auto-approve",
    // Permission mode — harness runs non-interactive; overriding
    // could silently expand auth (`auto` gives write access without
    // prompts).
    "--permission-mode",
    // System prompt — the harness lets cases set prompt content via
    // `prompt:`; allowing --system-prompt would bypass the judger's
    // anti-gaming preamble assumptions.
    "--system-prompt",
    // Session ID — the harness manages --session-id for multi-turn
    // steps; a case overriding it would break session continuation.
    "--session-id",
];

/// Validate that `args` does not contain any reserved flag. Returns
/// the first offender so the error message is precise. Matches both
/// exact form (`--model`) and `=` syntax (`--model=gpt-4`) so the
/// denylist cannot be bypassed by appending `=value`.
pub(crate) fn validate_extra_cli_args(args: &[String]) -> Result<(), String> {
    for a in args {
        let flag_part = a.split('=').next().unwrap_or(a);
        for r in RESERVED_CLI_ARGS {
            if flag_part == *r {
                return Err(format!(
                    "reserved CLI flag {a:?} in extra_cli_args — the harness \
                     manages this flag; pick a different mechanism (adjust \
                     prompt text, use a different harness flag, or open an \
                     issue if you need legitimate override support)"
                ));
            }
        }
    }
    Ok(())
}

/// Simple glob filter: `*` matches any sequence, `?` matches one char.
/// Used by both the CLI `--filter` and the dashboard filter.
pub fn matches_filter(name: &str, pattern: &str) -> bool {
    let regex_str = format!(
        "^{}$",
        regex::escape(pattern)
            .replace(r"\*", ".*")
            .replace(r"\?", ".")
    );
    regex::Regex::new(&regex_str)
        .map(|re| re.is_match(name))
        .unwrap_or(false)
}

impl Case {
    /// Load a case from a YAML file on disk.
    pub fn from_path(path: &Path) -> Result<Self, anyhow::Error> {
        // Cheap size guard so a pathological YAML (or a wrong file
        // type accidentally named `.yaml`) doesn't turn into an OOM.
        // 10 MiB is generous — the largest real case in the suite
        // fits in 3 KiB.
        const MAX_CASE_BYTES: u64 = 10 * 1024 * 1024;
        if let Ok(meta) = std::fs::metadata(path)
            && meta.len() > MAX_CASE_BYTES
        {
            anyhow::bail!(
                "{}: {} bytes exceeds the {} MiB case size cap — is this really a YAML case?",
                path.display(),
                meta.len(),
                MAX_CASE_BYTES / (1024 * 1024),
            );
        }
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let case: Case = serde_yaml_ng::from_str(&src).map_err(|e| {
            let msg = e.to_string();
            // Unknown-variant errors from serde look like
            //   `criteria[N].type: unknown variant 'xyz', expected one of …`
            // Point the case author at the README + the right
            // Criterion reference so the fix path is obvious.
            if msg.contains("unknown variant") {
                anyhow::anyhow!(
                    "parse {}: {msg}\n\
                     (see crates/astra-test-harness/README.md \
                     'Criteria types' for the current list — a recent \
                     rename may have deprecated the old name)",
                    path.display(),
                )
            } else {
                anyhow::anyhow!("parse {}: {msg}", path.display())
            }
        })?;
        let mut case = case;
        if case.name.trim().is_empty() {
            anyhow::bail!("case {}: name must not be empty", path.display());
        }
        if case.prompt.trim().is_empty() {
            anyhow::bail!("case {}: prompt must not be empty", path.display());
        }
        validate_prompt_variants(&case)
            .map_err(|error| anyhow::anyhow!("case {}: {error}", path.display()))?;
        if let Some(models) = case.models.clone() {
            case.models = Some(
                canonicalize_model_ids(&models)
                    .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?,
            );
        }
        // Fail fast on reserved-flag abuse so a typo in one case
        // doesn't silently poison an entire suite run.
        validate_extra_cli_args(&case.extra_cli_args)
            .map_err(|e| anyhow::anyhow!("case {}: {e}", path.display()))?;
        // `timeout_seconds: 0` collapses `Duration::from_secs(0)` —
        // every case would instantly report synthetic exit 124 before
        // the child even runs. A YAML typo turns the whole suite into
        // "harness hang". Require at least 1 second; realistic cases
        // want tens or hundreds.
        if !case.weight.is_finite() || case.weight < 0.0 {
            anyhow::bail!(
                "case {}: weight must be finite and >= 0.0 (got {})",
                path.display(),
                case.weight,
            );
        }
        if let Some(d) = case.difficulty
            && !(1..=5).contains(&d)
        {
            anyhow::bail!("case {}: difficulty must be 1–5 (got {d})", path.display(),);
        }
        if case.timeout_seconds == 0 {
            anyhow::bail!(
                "case {}: timeout_seconds must be >= 1 (got 0 — every case would \
                 time out before the child runs)",
                path.display(),
            );
        }
        // Reject criteria with internally-inconsistent bounds
        // (min>max, threshold>1.0, empty expect lists, bad regex).
        // A case author's YAML typo would otherwise turn into a
        // permanent-FAIL / permanent-PASS that looks like a real
        // regression.
        crate::criteria::validate_criteria(&case.criteria)
            .map_err(|e| anyhow::anyhow!("case {}: {e}", path.display()))?;
        for (i, step) in case.steps.iter().enumerate() {
            if step.prompt.trim().is_empty() {
                anyhow::bail!(
                    "case {}: steps[{i}].prompt must not be empty",
                    path.display()
                );
            }
            if step.timeout_seconds == Some(0) {
                anyhow::bail!(
                    "case {}: steps[{i}].timeout_seconds must be >= 1",
                    path.display()
                );
            }
            // Validate step criteria at load time.
            if let Err(e) = crate::criteria::validate_criteria(&step.criteria) {
                anyhow::bail!("case {}: steps[{i}].{e}", path.display(),);
            }
            if crate::criteria::requires_session_capture(&step.criteria) {
                anyhow::bail!(
                    "case {}: steps[{i}] contains a session/journal criterion; \
                     step outcomes are evaluated before the complete session is captured, so \
                     move this assertion to case-level criteria",
                    path.display(),
                );
            }
        }
        Ok(case)
    }

    /// Load every `*.yaml` / `*.yml` in a directory. Non-YAML files
    /// are skipped. Result is sorted by case `name` for deterministic
    /// suite-report ordering across filesystems and developer
    /// machines — previously `read_dir` order was OS-dependent and
    /// two developers could see different report orderings for the
    /// same suite.
    pub fn load_dir(dir: &Path) -> Result<Vec<Case>, anyhow::Error> {
        // Collect + sort the file paths first, so a parse error on a
        // later file deterministically reports the same neighbour.
        let mut paths: Vec<std::path::PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("read_dir {}: {e}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }
            paths.push(path);
        }
        // Path sort is load-order only: if `Case::from_path` errors
        // on one of the files, path-sorting makes the error
        // deterministic across filesystems ("every dev sees the same
        // first-failing file"). The authoritative report-order sort
        // happens on `out` after parsing, by in-case `name:` — a
        // YAML renamed without bumping `name:` shouldn't shift its
        // report row.
        paths.sort();

        let mut out: Vec<Case> = Vec::with_capacity(paths.len());
        for path in paths {
            out.push(Case::from_path(&path)?);
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(duplicate) = out
            .windows(2)
            .find(|pair| pair[0].name == pair[1].name)
            .map(|pair| pair[0].name.clone())
        {
            anyhow::bail!(
                "case suite {} contains duplicate case name {duplicate:?}",
                dir.display()
            );
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_minimal_case() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(&path, "name: hello\nprompt: just say ok\n").unwrap();
        let c = Case::from_path(&path).unwrap();
        assert_eq!(c.name, "hello");
        assert_eq!(c.prompt, "just say ok");
        assert!(c.criteria.is_empty());
        assert_eq!(c.timeout_seconds, 180);
        assert!(!c.debug_log);
    }

    #[test]
    fn prompt_variants_expand_with_shared_oracle_and_preserved_total_weight() {
        let mut cases = vec![
            serde_yaml_ng::from_str::<Case>(
                r#"name: semantic-contract
prompt: canonical wording
weight: 6.0
prompt_variants:
  - id: zh
    prompt: 等价中文表达
  - id: reordered
    step_index: 0
    prompt: same intent in a different order
steps:
  - prompt: canonical follow-up
criteria:
  - type: text_contains
    needle: receipt
"#,
            )
            .expect("variant case"),
        ];
        assert!(
            serde_json::to_value(&cases[0])
                .expect("case serializes")
                .get("prompt_variants")
                .is_none(),
            "harness-only metamorphic metadata must not change the executor protocol"
        );

        let added = expand_prompt_variants(&mut cases).expect("variants expand");

        assert_eq!(added, 2);
        assert_eq!(
            cases
                .iter()
                .map(|case| case.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "semantic-contract",
                "semantic-contract@reordered",
                "semantic-contract@zh"
            ]
        );
        assert!((cases.iter().map(|case| case.weight).sum::<f64>() - 6.0).abs() < f64::EPSILON);
        assert!(cases.iter().all(|case| case.criteria.len() == 1));
        assert!(cases.iter().all(|case| case.prompt_variants.is_empty()));
        assert_eq!(cases[0].steps[0].prompt, "canonical follow-up");
        assert_eq!(cases[1].steps[0].prompt, "same intent in a different order");
        assert_eq!(cases[2].prompt, "等价中文表达");
    }

    #[test]
    fn case_rejects_fake_or_ambiguous_prompt_variants() {
        let dir = tempdir().unwrap();
        for (filename, variants) in [
            (
                "duplicate.yaml",
                "  - id: same\n    prompt: canonical wording\n",
            ),
            (
                "unsafe-id.yaml",
                "  - id: ../escape\n    prompt: equivalent wording\n",
            ),
            (
                "bad-step.yaml",
                "  - id: missing-step\n    step_index: 0\n    prompt: equivalent wording\n",
            ),
        ] {
            let path = dir.path().join(filename);
            std::fs::write(
                &path,
                format!(
                    "name: semantic-contract\nprompt: canonical wording\nprompt_variants:\n{variants}"
                ),
            )
            .unwrap();
            assert!(Case::from_path(&path).is_err(), "{filename} must fail");
        }
    }

    #[test]
    fn case_rejects_empty_or_zero_timeout_follow_up() {
        let dir = tempdir().unwrap();
        for (filename, step) in [
            ("empty-step.yaml", "  - prompt: '   '\n"),
            (
                "zero-timeout.yaml",
                "  - prompt: follow up\n    timeout_seconds: 0\n",
            ),
        ] {
            let path = dir.path().join(filename);
            std::fs::write(
                &path,
                format!("name: semantic-contract\nprompt: canonical wording\nsteps:\n{step}"),
            )
            .unwrap();

            assert!(Case::from_path(&path).is_err(), "{filename} must fail");
        }
    }

    #[test]
    fn load_dir_rejects_duplicate_names_for_every_suite() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), "name: same\nprompt: one\n").unwrap();
        std::fs::write(dir.path().join("b.yaml"), "name: same\nprompt: two\n").unwrap();

        let error = Case::load_dir(dir.path()).expect_err("duplicate names must fail at load");
        assert!(error.to_string().contains("duplicate case name \"same\""));
    }

    #[test]
    fn bundled_introspection_reflection_case_keeps_durable_checks_strict() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("cases/introspection_reflection_source_boundary.yaml");
        let case = Case::from_path(&path).expect("bundled diagnostic case must parse");

        assert!(case.debug_log, "journal criteria require session capture");
        assert!(case
            .criteria
            .iter()
            .any(|criterion| matches!(criterion, crate::criteria::Criterion::JournalToolCallCount { name, min: 1, max: 1, .. } if name == "introspect")));
        assert!(case
            .criteria
            .iter()
            .any(|criterion| matches!(criterion, crate::criteria::Criterion::JournalToolCallCount { name, min: 1, max: 1, .. } if name == "reflect")));
    }

    #[test]
    fn bundled_memory_lifecycle_requires_purge_verification() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/memory_full_lifecycle.yaml");
        let case = Case::from_path(&path).expect("bundled memory lifecycle case must parse");

        assert!(case.criteria.iter().any(|criterion| matches!(
            criterion,
                crate::criteria::Criterion::JournalToolCallCount {
                    name,
                    min: 1,
                    max: 1,
                document: Some(crate::criteria::JournalToolDocument::Arguments),
                path: Some(path),
                equals: Some(value),
            } if name == "memory" && path == "/action" && value == "forget"
        )));
        let has_memory_flow = |consumer_document, consumer_paths, action| {
            case.criteria.iter().any(|criterion| {
                matches!(
                    criterion,
                    crate::criteria::Criterion::JournalToolValueFlowBound {
                        producer,
                        producer_document: crate::criteria::JournalToolDocument::Result,
                        producer_path,
                        producer_filters,
                        consumer,
                        consumer_document: actual_consumer_document,
                        consumer_paths: actual_consumer_paths,
                        consumer_filters,
                    } if producer == "memory"
                        && consumer == "memory"
                        && producer_path == "/memory_id"
                        && producer_filters.iter().any(|filter| {
                            filter.document == crate::criteria::JournalToolDocument::Arguments
                                && filter.path == "/action"
                                && filter.equals == serde_json::json!("remember")
                        })
                        && producer_filters.iter().any(|filter| {
                            filter.document == crate::criteria::JournalToolDocument::Arguments
                                && filter.path == "/memory_type"
                                && filter.equals == serde_json::json!("working")
                        })
                    && *actual_consumer_document == consumer_document
                    && *actual_consumer_paths == consumer_paths
                        && consumer_filters.iter().any(|filter| {
                            filter.document == crate::criteria::JournalToolDocument::Arguments
                                && filter.path == "/action"
                                && filter.equals == serde_json::json!(action)
                        })
                        && (action != "recall"
                            || consumer_filters.iter().any(|filter| {
                                filter.document == crate::criteria::JournalToolDocument::Arguments
                                    && filter.path == "/scope"
                                    && filter.equals == serde_json::json!("session")
                            }))
                )
            })
        };
        assert!(
            has_memory_flow(
                crate::criteria::JournalToolDocument::Result,
                vec!["/*/memory_id".to_string()],
                "recall",
            ),
            "memory lifecycle must prove remember->recall ID provenance"
        );
        assert!(
            has_memory_flow(
                crate::criteria::JournalToolDocument::Arguments,
                vec!["/memory_id".to_string(), "/memory_ids".to_string()],
                "forget",
            ),
            "memory lifecycle must prove remember->forget ID provenance"
        );
        assert!(case.cleanup_memory_records);
    }

    #[test]
    fn rejects_step_level_session_criteria_before_execution() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("step-session.yaml");
        std::fs::write(
            &path,
            r#"name: invalid-step-session
prompt: first
steps:
  - prompt: second
    criteria:
      - type: journal_tool_called
        name: memory
"#,
        )
        .unwrap();

        let error = Case::from_path(&path).expect_err("step journal criteria must fail fast");
        assert!(
            error
                .to_string()
                .contains("steps[0] contains a session/journal criterion")
        );
    }

    #[test]
    fn bundled_active_run_tool_result_case_makes_efficiency_bounds_hard() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("cases/active_run_tool_result_retention.yaml");
        let case = Case::from_path(&path).expect("bundled active-run case must parse");

        let hard_group = case
            .criteria
            .iter()
            .find_map(|criterion| match criterion {
                crate::criteria::Criterion::AllOf { criteria } => Some(criteria),
                _ => None,
            })
            .expect("tool and round bounds must be nested in all_of");

        assert!(
            hard_group.iter().any(|criterion| matches!(
                criterion,
                crate::criteria::Criterion::ToolsCountBetween { min: 8, max: 8 }
            )),
            "the scenario must fail when the model skips or invents tool work"
        );
        assert!(
            hard_group.iter().any(|criterion| matches!(
                criterion,
                crate::criteria::Criterion::TurnRoundsBetween { min: 2, max: 15 }
            )),
            "the scenario must fail on an inefficient multi-round loop"
        );
        let exact_read_paths = case
            .criteria
            .iter()
            .filter(|criterion| {
                matches!(
                    criterion,
                    crate::criteria::Criterion::JournalToolCallCount {
                        name,
                        min: 1,
                        max: 1,
                        document: Some(crate::criteria::JournalToolDocument::Arguments),
                        path: Some(path),
                        equals: Some(_),
                    } if name == "read_file" && path == "/path"
                )
            })
            .count();
        assert_eq!(
            exact_read_paths, 8,
            "all eight requested reads must be proved by durable argument evidence"
        );
        assert!(
            !case.prompt.contains("DASHMAP_VERSION: 6.1.0"),
            "the capability prompt must not disclose the answer it is meant to recover"
        );
    }

    #[test]
    fn ordinary_tool_cases_do_not_claim_compaction() {
        fn claims_compaction(criterion: &crate::criteria::Criterion) -> bool {
            match criterion {
                crate::criteria::Criterion::SessionEventCount { event_type, .. } => {
                    event_type == "CompactionFired"
                }
                crate::criteria::Criterion::AnyOf { criteria }
                | crate::criteria::Criterion::AllOf { criteria } => {
                    criteria.iter().any(claims_compaction)
                }
                _ => false,
            }
        }

        let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("cases");
        for name in [
            "multi_turn_read_reuse.yaml",
            "active_run_tool_result_retention.yaml",
            "multi_turn_tool_pairing_integrity.yaml",
        ] {
            let case = Case::from_path(&cases_dir.join(name)).expect("tool case must parse");
            assert!(
                !case.criteria.iter().any(claims_compaction)
                    && !case
                        .steps
                        .iter()
                        .flat_map(|step| &step.criteria)
                        .any(claims_compaction),
                "{name} exercises ordinary tool continuity and must not claim compaction"
            );
        }
    }

    #[test]
    fn bundled_work_journey_has_strict_protocol_and_interaction_contract() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("cases/work_semantic_delegation_order.yaml");
        let case = Case::from_path(&path).expect("bundled Work journey must parse");

        assert!(
            case.criteria.iter().any(|criterion| matches!(
                criterion,
                crate::criteria::Criterion::JournalToolOutcomeCount {
                    name,
                    ok: false,
                    min: 0,
                    max: 0,
                } if name == "start_work"
            )),
            "the Work journey must reject failed lifecycle initialization from durable typed evidence"
        );
        assert!(
            case.criteria.iter().any(|criterion| matches!(
                criterion,
                crate::criteria::Criterion::ProviderPromptCacheReadRatio {
                    min,
                    warmup_turns: 0,
                    warmup_rounds: 1,
                } if (*min - 0.95).abs() < f64::EPSILON
            )),
            "the Work journey must report the 95% provider prompt-cache quality target after warmup"
        );
    }

    #[test]
    fn case_rejects_unknown_fields_instead_of_ignoring_a_misspelled_contract() {
        let error = serde_yaml_ng::from_str::<Case>(
            "name: strict\nprompt: hello\ncleanup_memory_record: true\n",
        )
        .expect_err("unknown case fields must fail at load time");
        assert!(
            error.to_string().contains("cleanup_memory_record"),
            "{error}"
        );
    }

    #[test]
    fn case_schema_names_subprocess_environment_as_cli_only() {
        let case = serde_yaml_ng::from_str::<Case>(
            "name: cli-env\nprompt: hello\ncli_env:\n  ASTRA_TRACE: verbose\n",
        )
        .expect("explicit CLI environment must parse");
        assert_eq!(
            case.cli_env.get("ASTRA_TRACE").map(String::as_str),
            Some("verbose")
        );

        let error = serde_yaml_ng::from_str::<Case>(
            "name: ambiguous-env\nprompt: hello\nenv:\n  ASTRA_TRACE: verbose\n",
        )
        .expect_err("ambiguous env must not look like Server configuration");
        assert!(error.to_string().contains("env"), "{error}");
    }

    #[test]
    fn every_bundled_case_parses_with_the_strict_criterion_schema() {
        let cases_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("cases");
        let cases = Case::load_dir(&cases_dir).expect("all bundled cases must parse");
        assert!(!cases.is_empty());
    }

    #[test]
    fn load_dir_skips_non_yaml() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.yaml"), "name: a\nprompt: p\n").unwrap();
        std::fs::write(dir.path().join("b.yml"), "name: b\nprompt: p\n").unwrap();
        std::fs::write(dir.path().join("readme.md"), "# notes\n").unwrap();
        let cases = Case::load_dir(dir.path()).unwrap();
        assert_eq!(cases.len(), 2);
    }

    #[test]
    fn load_dir_returns_cases_sorted_by_name_for_reproducible_reports() {
        // Filenames intentionally NOT in alphabetical name order so
        // the sort has to do real work; case names also differ from
        // filenames to exercise the secondary by-name sort.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("zzz.yaml"), "name: alpha\nprompt: p\n").unwrap();
        std::fs::write(dir.path().join("aaa.yaml"), "name: charlie\nprompt: p\n").unwrap();
        std::fs::write(dir.path().join("mmm.yaml"), "name: bravo\nprompt: p\n").unwrap();
        let cases = Case::load_dir(dir.path()).unwrap();
        let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "bravo", "charlie"],
            "load_dir must sort by case name, not by filename; got {names:?}"
        );
    }

    #[test]
    fn parse_fails_loudly_on_missing_required_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "description: no name or prompt\n").unwrap();
        assert!(Case::from_path(&path).is_err());
    }

    // ── Reserved CLI flag rejection (Review #5) ──

    #[test]
    fn parse_rejects_reserved_flags_in_extra_cli_args() {
        // Covers the full denylist including flags surfaced by R2 #4:
        //  - top-level `--yes` (astra --yes)
        //  - chat `--auto-approve` (new canonical name for -y on chat)
        //  - `--permission-mode` (silently expands tool auth)
        //  - `--stdin` (would redirect prompt input to stdin)
        //  - `--system-prompt` (bypasses judger's anti-gaming preamble)
        for reserved in [
            "-m",
            "--message",
            "--stdin",
            "--model",
            "--json",
            "--quiet",
            "-y",
            "--yes",
            "--auto-approve",
            "--permission-mode",
            "--system-prompt",
        ] {
            let dir = tempdir().unwrap();
            let path = dir.path().join("bad.yaml");
            let yaml =
                format!("name: c\nprompt: p\nextra_cli_args: [{reserved:?}, \"something\"]\n");
            std::fs::write(&path, yaml).unwrap();
            let err = Case::from_path(&path)
                .err()
                .unwrap_or_else(|| panic!("{reserved} was accepted"));
            let msg = err.to_string();
            assert!(
                msg.contains(reserved) && msg.contains("reserved"),
                "error should name the reserved flag {reserved}: {msg}"
            );
        }
    }

    #[test]
    fn reserved_list_does_not_include_nonexistent_flags() {
        // Regression guard: `--approve-all` was in the original
        // denylist but is not a real flag. Removing dead entries
        // keeps the error message actionable for cases that hit it.
        assert!(!RESERVED_CLI_ARGS.contains(&"--approve-all"));
    }

    #[test]
    fn unknown_criterion_variant_error_points_at_readme() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.yaml");
        std::fs::write(
            &path,
            "name: c\nprompt: p\ncriteria:\n  - type: fork_cache_class\n    expect: [hit]\n",
        )
        .unwrap();
        let err = Case::from_path(&path).expect_err("old type name must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("unknown variant"),
            "serde's well-shaped error is preserved: {msg}"
        );
        assert!(
            msg.contains("fork_cache_class"),
            "offending name must appear: {msg}"
        );
        assert!(
            msg.contains("README.md"),
            "remediation hint must appear: {msg}"
        );
    }

    #[test]
    fn oversize_case_file_rejected_before_read() {
        // A YAML "file" of many megabytes is almost certainly a typo
        // (someone pointed at a log / binary). Cap protects against
        // OOM on shared suite dirs.
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.yaml");
        // Write just over 10 MiB of padding — invalid YAML inside
        // doesn't matter because the size guard fires first.
        let big = vec![b'a'; 10 * 1024 * 1024 + 1];
        std::fs::write(&path, &big).unwrap();
        let err = Case::from_path(&path).expect_err("oversize must reject");
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn timeout_seconds_zero_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ntimeout_seconds: 0\n").unwrap();
        let err = Case::from_path(&path).expect_err("timeout_seconds=0 must fail");
        assert!(err.to_string().contains("timeout_seconds must be >= 1"));
    }

    #[test]
    fn criteria_bounds_validated_at_load() {
        // End-to-end of R3 #2: a YAML with a bad-bounds criterion
        // must fail at parse time, not at evaluation time.
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(
            &path,
            "name: c\nprompt: p\ncriteria:\n  - type: tools_count_between\n    min: 5\n    max: 2\n",
        )
        .unwrap();
        let err = Case::from_path(&path).expect_err("inverted bounds must fail");
        let msg = err.to_string();
        assert!(msg.contains("min (5) > max (2)"), "{msg}");
    }

    #[test]
    fn parse_accepts_non_reserved_extra_cli_args() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.yaml");
        std::fs::write(
            &path,
            "name: c\nprompt: p\nextra_cli_args: [\"--verbose\", \"--explain\"]\n",
        )
        .unwrap();
        let c = Case::from_path(&path).expect("non-reserved flags should be accepted");
        assert_eq!(
            c.extra_cli_args,
            vec!["--verbose".to_string(), "--explain".to_string()]
        );
    }

    #[test]
    fn validate_extra_cli_args_accepts_empty_and_unknown() {
        assert!(validate_extra_cli_args(&[]).is_ok());
        assert!(validate_extra_cli_args(&["--whatever".into()]).is_ok());
    }

    #[test]
    fn reserved_flag_bypass_via_equals_syntax_rejected() {
        for bypass in [
            "--model=gpt-4",
            "--message=hello",
            "--json=true",
            "--permission-mode=auto",
            "--system-prompt=override",
            "--session-id=hijack",
        ] {
            let err = validate_extra_cli_args(&[bypass.into()]);
            assert!(
                err.is_err(),
                "{bypass:?} should be rejected (= syntax bypass)"
            );
        }
    }

    #[test]
    fn non_reserved_flag_with_equals_accepted() {
        assert!(validate_extra_cli_args(&["--verbose=true".into()]).is_ok());
        assert!(validate_extra_cli_args(&["--explain=yes".into()]).is_ok());
    }

    #[test]
    fn negative_weight_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "name: c\nprompt: p\nweight: -1.0\n").unwrap();
        let err = Case::from_path(&path).expect_err("negative weight must fail");
        assert!(err.to_string().contains("weight"), "{err}");
    }

    #[test]
    fn difficulty_out_of_range_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ndifficulty: 200\n").unwrap();
        let err = Case::from_path(&path).expect_err("difficulty 200 must fail");
        assert!(err.to_string().contains("difficulty"), "{err}");
    }

    #[test]
    fn difficulty_zero_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ndifficulty: 0\n").unwrap();
        let err = Case::from_path(&path).expect_err("difficulty 0 must fail");
        assert!(err.to_string().contains("difficulty"), "{err}");
    }

    #[test]
    fn capability_typo_warns_on_near_miss() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("typo.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ncapability: Tool_Use\n").unwrap();
        let err = Case::from_path(&path).expect_err("near-miss capability must fail");
        assert!(
            err.to_string().contains("unknown capability"),
            "error should reject unknown variant: {err}"
        );
    }

    #[test]
    fn capability_known_variant_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ncapability: tool_use\n").unwrap();
        let c = Case::from_path(&path).expect("known capability");
        assert_eq!(c.capability, Some(Capability::ToolUse));
    }

    #[test]
    fn capability_truly_custom_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.yaml");
        std::fs::write(
            &path,
            "name: c\nprompt: p\ncapability: \"custom:my_special_cap\"\n",
        )
        .unwrap();
        let c = Case::from_path(&path).expect("custom capability");
        assert_eq!(
            c.capability,
            Some(Capability::Custom("my_special_cap".into()))
        );
    }

    #[test]
    fn capability_bare_unknown_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "name: c\nprompt: p\ncapability: fast_response\n").unwrap();
        let err = Case::from_path(&path).expect_err("bare unknown must fail");
        assert!(
            err.to_string().contains("unknown capability"),
            "should reject: {err}"
        );
    }

    #[test]
    fn valid_weight_and_difficulty_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.yaml");
        std::fs::write(&path, "name: c\nprompt: p\nweight: 2.5\ndifficulty: 3\n").unwrap();
        let c = Case::from_path(&path).expect("valid weight+difficulty");
        assert!((c.weight - 2.5).abs() < f64::EPSILON);
        assert_eq!(c.difficulty, Some(3));
    }

    // ── matches_filter (glob) ──

    #[test]
    fn matches_filter_star_glob() {
        assert!(matches_filter("fork_prefix_hit", "fork_*"));
        assert!(!matches_filter("hello_world", "fork_*"));
    }

    #[test]
    fn matches_filter_question_mark_glob() {
        assert!(matches_filter("abc", "a?c"));
        assert!(!matches_filter("abbc", "a?c"));
    }

    #[test]
    fn matches_filter_exact_match() {
        assert!(matches_filter("hello", "hello"));
        assert!(!matches_filter("hello_world", "hello"));
    }
}
