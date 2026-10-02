//! Artifact persistence: write per-case outputs to a structured directory.
//!
//! Layout: `<artifacts_dir>/<case_name>/<model>/<run_index>/`
//!   - stdout.txt
//!   - stderr.txt
//!   - report.json (CaseRunReport)
//!   - digest.json (if available)
//!   - stream-events.json (private, before owned session deletion)

use std::path::{Path, PathBuf};

use crate::report::{AttemptRecord, CaseRunReport, StepResult};

fn artifact_dir(base_dir: &Path, case_name: &str, model: &str, run_index: u32) -> PathBuf {
    base_dir
        .join(encode_component(case_name))
        .join(encode_component(model))
        .join(run_index.to_string())
}

/// Persist physical executions without serializing prompts or aggregate output.
/// Failure leaves session deletion with the suite's existing cleanup owner.
pub(crate) fn persist_stream_capture(
    base_dir: &Path,
    case_name: &str,
    model: &str,
    run_index: u32,
    attempts: &[AttemptRecord],
    steps: &[StepResult],
) -> std::io::Result<()> {
    #[derive(serde::Serialize)]
    struct Execution<'a> {
        kind: &'static str,
        index: u32,
        model: &'a str,
        exit_code: i32,
        final_state: Option<&'a str>,
        capture: Option<&'a crate::runner::StreamCapture>,
    }

    let mut executions = Vec::new();
    for (kind, index, outcome) in attempts
        .iter()
        .map(|attempt| ("attempt", attempt.attempt_index, &attempt.outcome))
        .chain(
            steps
                .iter()
                .map(|step| ("step", step.step_index, &step.outcome)),
        )
    {
        if outcome.session_id.is_some() && outcome.stream_capture.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("missing stream capture for {kind} {index}"),
            ));
        }
        executions.push(Execution {
            kind,
            index,
            model: &outcome.model,
            exit_code: outcome.exit_code,
            final_state: outcome.final_state.as_deref(),
            capture: outcome.stream_capture.as_ref(),
        });
    }

    let dir = artifact_dir(base_dir, case_name, model, run_index);
    for ancestor in dir.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "stream artifact directory contains a symlink",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut file = tempfile::NamedTempFile::new_in(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(file.as_file_mut(), &executions)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    file.as_file().sync_all()?;
    file.persist(dir.join("stream-events.json"))
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

/// Write artifacts for a single case run to the given base directory.
pub fn persist_artifacts(base_dir: &Path, report: &CaseRunReport) -> std::io::Result<()> {
    let dir = artifact_dir(base_dir, &report.case_name, &report.model, report.run_index);
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

/// Encode UTF-8 bytes injectively; a lone '%' uniquely represents empty input.
fn encode_component(s: &str) -> String {
    if s.is_empty() {
        return "%".to_string();
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(s.len());
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunOutcome;

    #[test]
    fn persist_creates_directory_structure() {
        let tmp = tempfile::tempdir().unwrap();

        let mut report = CaseRunReport {
            case_name: "test/case".into(),
            model: "my.model".into(),
            status: crate::report::CaseRunStatus::Passed,
            run_index: 0,
            capability: None,
            weight: 1.0,
            difficulty: None,
            outcome: RunOutcome::new("my.model").with_text("hello"),
            criteria: vec![],
            steps: vec![],
            attempts: Vec::new(),
            session: None,
            session_captures: Vec::new(),
            execution: None,
            reproducer: None,
            digest: None,
            digest_error: None,
            failure_class: None,
            cleanup_errors: Vec::new(),
            has_warnings: false,
        };

        persist_artifacts(tmp.path(), &report).unwrap();
        let dir = tmp.path().join("test%2Fcase/my%2Emodel/0");
        assert!(dir.join("stdout.txt").exists());
        assert!(dir.join("report.json").exists());
        assert!(!dir.join("stream-events.json").exists());

        let mut stream = crate::runner::StreamCapture::default();
        stream.observe(
            &serde_json::json!({"type":"agent_live","event":{
                "run_id":"child","agent_id":"agent","kind":{
                    "type":"output_delta","model_item_id":null,"text":"private child\n"
                }
            }}),
            1,
        );
        report.outcome.stream_capture = Some(stream);
        report.attempts.push(AttemptRecord {
            attempt_index: 0,
            outcome: report.outcome.clone(),
        });
        report.steps.push(StepResult {
            step_index: 2,
            prompt: "excluded private prompt".into(),
            outcome: report.outcome.clone(),
            duration_ms: 0,
            criteria: vec![],
            passed: true,
        });
        persist_stream_capture(
            tmp.path(),
            &report.case_name,
            &report.model,
            0,
            &report.attempts,
            &report.steps,
        )
        .unwrap();
        persist_artifacts(tmp.path(), &report).unwrap();
        let bytes = std::fs::read(dir.join("stream-events.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved[0]["kind"], "attempt");
        assert_eq!(saved[0]["index"], 0);
        assert_eq!(saved[1]["kind"], "step");
        assert_eq!(saved[1]["index"], 2);
        assert_eq!(
            saved[0]["capture"]["records"][0]["event"]["kind"]["text"],
            "private child\n"
        );
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains("excluded private prompt")
        );
        assert!(
            !std::fs::read_to_string(dir.join("report.json"))
                .unwrap()
                .contains("private child")
        );
        persist_stream_capture(
            tmp.path(),
            &report.case_name,
            &report.model,
            1,
            &report.attempts,
            &report.steps,
        )
        .unwrap();
        assert!(
            tmp.path()
                .join("test%2Fcase/my%2Emodel/1/stream-events.json")
                .exists()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(dir.join("stream-events.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn stream_sidecar_rejects_symlink_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(foreign.path(), tmp.path().join("case")).unwrap();
        assert!(persist_stream_capture(tmp.path(), "case", "m", 0, &[], &[]).is_err());
        assert_eq!(std::fs::read_dir(foreign.path()).unwrap().count(), 0);
    }

    #[test]
    fn encode_component_empty_string_uses_unique_sentinel() {
        assert_eq!(encode_component(""), "%");
        assert_eq!(encode_component("%"), "%25");
        assert_eq!(encode_component("_"), "_");
    }

    #[test]
    fn encode_component_preserves_distinct_utf8_bytes() {
        for (input, expected) in [
            ("a/b", "a%2Fb"),
            ("a_b", "a_b"),
            ("a@b", "a%40b"),
            ("a%b", "a%25b"),
            ("%2F", "%252F"),
            ("你", "%E4%BD%A0"),
            ("hello-A_1", "hello-A_1"),
        ] {
            assert_eq!(encode_component(input), expected);
        }
    }

    #[test]
    fn stream_sidecars_preserve_colliding_case_names() {
        let tmp = tempfile::tempdir().unwrap();
        let names = [
            ("case@natural", "first-root"),
            ("case_natural", "second-root"),
        ];
        for (name, root) in names {
            let mut capture = crate::runner::StreamCapture::default();
            capture.root_run_id = Some(root.into());
            let mut outcome = RunOutcome::new("m");
            outcome.stream_capture = Some(capture);
            persist_stream_capture(
                tmp.path(),
                name,
                "m",
                0,
                &[AttemptRecord {
                    attempt_index: 0,
                    outcome,
                }],
                &[],
            )
            .unwrap();
        }
        for (component, root) in [
            ("case%40natural", "first-root"),
            ("case_natural", "second-root"),
        ] {
            let saved: serde_json::Value = serde_json::from_slice(
                &std::fs::read(tmp.path().join(component).join("m/0/stream-events.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(saved[0]["capture"]["root_run_id"], root);
        }
    }
}
