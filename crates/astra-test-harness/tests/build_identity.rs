use std::process::Command;

#[test]
fn executable_identity_precedes_configuration_and_argument_parsing() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .arg("--build-info-json")
        .current_dir(directory.path())
        .env(
            "ASTRA_CONFIG_SOURCE",
            "invalid-configuration-must-not-be-read",
        )
        .env("ASTRA_EXPECTED_BUILD_GIT_SHA", "invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::to_value(astra_core::build_info::current()).unwrap(),
    );
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn requested_revision_cannot_be_certified_with_skipped_preflight() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
        .arg("--skip-preflight")
        .current_dir(directory.path())
        .env(
            "ASTRA_EXPECTED_BUILD_GIT_SHA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot bypass"),
        "{output:?}"
    );
}

#[test]
fn requested_revision_rejects_execution_paths_without_identity_verification() {
    let directory = tempfile::tempdir().unwrap();
    for arguments in [["--live-dashboard", "17899"], ["--executor-cmd", "false"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
            .args(arguments)
            .current_dir(directory.path())
            .env(
                "ASTRA_EXPECTED_BUILD_GIT_SHA",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("requires the built-in CLI executor"),
            "{output:?}"
        );
    }
}
