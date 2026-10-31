use astra_core::history_work_baseline::{
    BuildAttestation, ProductionProcessCaptureGuard, ProductionProcessRole,
};
use std::{fs, process::Command};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NONCE: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn compiled_identity_reaches_the_canonical_verifier() {
    let build = astra_build_info::current_attestation();
    let info = astra_build_info::current();
    assert_eq!(build.git_sha, info.git_sha);
    assert_eq!(build.git_dirty == "true", info.git_dirty);
    assert_eq!(build.nonce, astra_build_info::BUILD_ATTESTATION_NONCE);
    let expected = astra_core::history_work_baseline::verify_current_build_attestation(
        info.git_sha,
        NONCE,
        build,
    );
    let actual = astra_build_info::verify_current_build_attestation(info.git_sha, NONCE);
    assert_eq!(
        actual.map_err(|e| e.to_string()),
        expected.map_err(|e| e.to_string())
    );
    assert!(astra_build_info::verify_current_build_attestation("unknown", NONCE).is_err());
}

#[test]
fn capture_accepts_clean_identity_and_rejects_invalid_identity_without_output() {
    let root = std::env::temp_dir().join(format!("astra-attestation-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    for case in ["valid", "sha", "dirty", "nonce", "unknown", "absent"] {
        let path = root.join(format!("{case}.json"));
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "capture_child", "--nocapture"])
            .env("ASTRA_ATTESTATION_TEST_CASE", case)
            .env("ASTRA_HISTORY_WORK_TRACE", "1")
            .env("ASTRA_HISTORY_WORK_BASELINE_FRAGMENT", &path)
            .env("ASTRA_HISTORY_WORK_BASELINE_RUN_ID", NONCE)
            .env("ASTRA_HISTORY_WORK_BASELINE_GIT_SHA", SHA)
            .env("ASTRA_HISTORY_WORK_BASELINE_CAPTURE_SCOPE", "\"setup\"")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{case}: {}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(path.exists(), case == "valid", "{case}");
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn capture_child() {
    let Ok(case) = std::env::var("ASTRA_ATTESTATION_TEST_CASE") else {
        return;
    };
    let build = BuildAttestation {
        git_sha: match case.as_str() {
            "sha" => "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "unknown" => "unknown",
            _ => SHA,
        },
        git_dirty: if case == "dirty" { "true" } else { "false" },
        nonce: match case.as_str() {
            "nonce" => "2222222222222222222222222222222222222222222222222222222222222222",
            "absent" => "absent",
            _ => NONCE,
        },
    };
    let result = ProductionProcessCaptureGuard::from_env(ProductionProcessRole::Server, build);
    if case == "valid" {
        let capture = result.unwrap().unwrap().finish().unwrap();
        assert_eq!(capture.git_sha, SHA);
        assert!(!capture.build_git_dirty);
    } else {
        assert!(result.is_err(), "invalid {case} accepted");
    }
}
