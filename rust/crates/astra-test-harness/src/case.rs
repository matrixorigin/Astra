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
    /// Flags are appended after all harness-managed flags; do NOT
    /// use this to override `-m`, `--model`, `--json`, `-y`.
    #[serde(default)]
    pub extra_cli_args: Vec<String>,

    /// Optional per-case timeout in seconds. Defaults to 180.
    /// Caps runaway models without silently skewing matrix totals.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_timeout_seconds() -> u64 {
    180
}

impl Case {
    /// Load a case from a YAML file on disk.
    pub fn from_path(path: &Path) -> Result<Self, anyhow::Error> {
        let src = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        let case: Case = serde_yaml_ng::from_str(&src)
            .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        Ok(case)
    }

    /// Load every `*.yaml` / `*.yml` in a directory. Non-YAML files
    /// are skipped. Order is filesystem-order (stable on most FS).
    pub fn load_dir(dir: &Path) -> Result<Vec<Case>, anyhow::Error> {
        let mut out = Vec::new();
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
            out.push(Case::from_path(&path)?);
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
        std::fs::write(
            &path,
            "name: hello\nprompt: just say ok\n",
        )
        .unwrap();
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
    fn parse_fails_loudly_on_missing_required_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        std::fs::write(&path, "description: no name or prompt\n").unwrap();
        assert!(Case::from_path(&path).is_err());
    }
}
