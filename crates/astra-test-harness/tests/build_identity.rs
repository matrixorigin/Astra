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

#[test]
fn case_routing_overrides_are_rejected_before_preflight_or_model_execution() {
    let directory = tempfile::tempdir().unwrap();
    let suite = directory.path().join("cases");
    std::fs::create_dir(&suite).unwrap();
    // Deliberately not executable: revision-bound case validation must fail
    // before even attempting the artifact/health/model probes of this CLI.
    let cli = directory.path().join("astra");
    std::fs::write(&cli, "must never be executed").unwrap();
    for configuration in [
        "cli_env: {ASTRA_API_URL: 'http://case-target.invalid'}",
        "cli_env: {ASTRA_PROFILE: other}",
        "cli_env: {https_proxy: 'http://case-proxy.invalid'}",
        "extra_cli_args: ['--api-url=http://case-target.invalid']",
        "extra_cli_args: ['--profile', other]",
    ] {
        std::fs::write(
            suite.join("routing.yaml"),
            format!(
                "name: routing\nprompt: hello\n{configuration}\nsteps:\n  - prompt: continue\n"
            ),
        )
        .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_astra-test"))
            .arg("--suite")
            .arg(&suite)
            .arg("--astra-bin")
            .arg(&cli)
            .args([
                "--profile",
                "local",
                "--models",
                "test-model",
                "--no-judger",
            ])
            .current_dir(directory.path())
            .env("ASTRA_API_URL", "http://preflight-target.invalid")
            .env(
                "ASTRA_EXPECTED_BUILD_GIT_SHA",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("can change the verified execution target"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("case-target.invalid"),
            "must not disclose values: {stderr}"
        );
        assert!(
            !stderr.contains("binary exists but is not executable"),
            "{stderr}"
        );
    }
}
