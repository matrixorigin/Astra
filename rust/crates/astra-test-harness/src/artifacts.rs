//! Artifact persistence: write per-case outputs to a structured directory.
//!
//! Layout: `<artifacts_dir>/<case_name>/<model>/<run_index>/`
//!   - stdout.txt
//!   - stderr.txt
//!   - report.json (CaseRunReport)
//!   - digest.json (if available)

use std::path::Path;

use crate::report::CaseRunReport;

/// Write artifacts for a single case run to the given base directory.
pub fn persist_artifacts(base_dir: &Path, report: &CaseRunReport) -> std::io::Result<()> {
    let dir = base_dir
        .join(sanitize(&report.case_name))
        .join(sanitize(&report.model))
        .join(report.run_index.to_string());
    std::fs::create_dir_all(&dir)?;

    std::fs::write(dir.join("stdout.txt"), &report.outcome.text)?;
    std::fs::write(dir.join("stderr.txt"), &report.outcome.stderr)?;

    let report_json = serde_json::to_string_pretty(report).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("report.json serialize failed: {e}"),
        )
    })?;
    std::fs::write(dir.join("report.json"), report_json)?;

    if let Some(ref digest) = report.digest {
        let digest_json = serde_json::to_string_pretty(&digest.json).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("digest.json serialize failed: {e}"),
            )
        })?;
        std::fs::write(dir.join("digest.json"), digest_json)?;
    }

    Ok(())
}

/// Sanitize a string for use as a directory name. Returns "_" for
/// empty strings to avoid creating a directory with an empty name.
fn sanitize(s: &str) -> String {
    if s.is_empty() {
        return "_".to_string();
    }
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunOutcome;

    #[test]
    fn persist_creates_directory_structure() {
        let tmp = std::env::temp_dir().join("astra_test_artifacts_test");
        let _ = std::fs::remove_dir_all(&tmp);

        let report = CaseRunReport {
            case_name: "test/case".into(),
            model: "my.model".into(),
            passed: true,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("my.model").with_text("hello"),
            criteria: vec![],
            steps: vec![],
            session: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
        };

        persist_artifacts(&tmp, &report).unwrap();
        assert!(tmp.join("test_case/my_model/0/stdout.txt").exists());
        assert!(tmp.join("test_case/my_model/0/report.json").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sanitize_empty_string_returns_underscore() {
        assert_eq!(super::sanitize(""), "_");
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(super::sanitize("a/b.c"), "a_b_c");
        assert_eq!(super::sanitize("hello"), "hello");
    }
}
