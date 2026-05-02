//! YAML case loader.
//!
//! Each case is a single YAML document describing one behavior we
//! want to verify, independent of the model that implements it.
//! Cases are runnable across a model matrix — the same prompt is
//! replayed against each model in `models:` (or the CLI-provided
//! fallback list).
//!
//! See `cases/spawn_agent_hello.yaml` for a worked example.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A single test case. One YAML file == one `Case`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Shell command to run before the case (e.g., create temp files).
    /// Runs in `working_dir` or CWD. Non-zero exit aborts the case.
    #[serde(default)]
    pub setup_cmd: Option<String>,

    /// Shell command to run after the case (cleanup). Always runs,
    /// even on failure.
    #[serde(default)]
    pub teardown_cmd: Option<String>,
}

/// A follow-up turn in a multi-turn case.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
                     (see rust/crates/astra-test-harness/README.md \
                     'Criteria types' for the current list — a recent \
                     rename may have deprecated the old name)",
                    path.display(),
                )
            } else {
                anyhow::anyhow!("parse {}: {msg}", path.display())
            }
        })?;
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
            // Validate step criteria at load time.
            if let Err(e) = crate::criteria::validate_criteria(&step.criteria) {
                anyhow::bail!("case {}: steps[{i}].{e}", path.display(),);
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
