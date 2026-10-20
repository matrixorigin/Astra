//! Exercise the documented local setup commands without a Server or credentials.
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_astra"));
    command
        .args(args)
        .current_dir(root.join("project"))
        .env("HOME", root.join("home"))
        .env("ASTRA_LOCAL_STATE_ROOT", root.join("user"))
        .env_remove("RUST_LOG");
    command
}

fn run(root: &Path, args: &[&str]) -> Output {
    command(root, args).output().expect("run local MCP command")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn local_setup_reuses_scopes_and_preserves_secrets_on_failures() {
    let root = tempfile::tempdir().unwrap();
    for dir in ["project", "home"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    let root = root.path();
    for (scope, parent) in [
        ("project", root.join("project/.astra")),
        ("user", root.join("user")),
    ] {
        let missing = run(root, &["mcp", "remove", "missing", "-s", scope]);
        assert_eq!(missing.status.code(), Some(3), "{}", text(&missing));
        assert!(
            !parent.exists(),
            "failed remove created {}",
            parent.display()
        );
        std::fs::create_dir(&parent).unwrap();
        let missing = run(root, &["mcp", "remove", "missing", "-s", scope]);
        assert_eq!(missing.status.code(), Some(3), "{}", text(&missing));
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        let lock = parent.join("mcp.json.lock");
        std::fs::write(&lock, "existing-lock").unwrap();
        let missing = run(root, &["mcp", "remove", "missing", "-s", scope]);
        assert_eq!(missing.status.code(), Some(3), "{}", text(&missing));
        assert_eq!(std::fs::read_to_string(lock).unwrap(), "existing-lock");
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 1);
    }
    let help = run(root, &["mcp", "--help"]);
    assert!(help.status.success(), "{}", text(&help));
    assert!(text(&help).contains(".astra/mcp.json"));
    let secret = "mcp-fixture-private-value";
    let remote = serde_json::json!({
        "type":"streamable_http", "url":"https://example.invalid/mcp",
        "headers":{"Authorization":secret}
    })
    .to_string();
    let added = run(root, &["mcp", "add-json", "remote", &remote, "-s", "user"]);
    assert!(added.status.success(), "{}", text(&added));
    assert!(!text(&added).contains(secret));
    let local = serde_json::json!({"command":secret}).to_string();
    let added = run(
        root,
        &["mcp", "add-json", "remote", &local, "-s", "project"],
    );
    assert!(added.status.success(), "{}", text(&added));
    let config_path = root.join("project/.astra/mcp.json");
    let before = std::fs::read(&config_path).unwrap();
    for entry in [&local, &format!("{{\"command\":\"{secret}\",")] {
        let rejected = run(root, &["mcp", "add-json", "remote", entry, "-s", "project"]);
        assert!(!rejected.status.success());
        assert!(!text(&rejected).contains(secret));
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
    }
    for args in [
        vec!["mcp", "list", "--effective"],
        vec!["mcp", "get", "remote"],
    ] {
        let inspected = run(root, &args);
        assert!(inspected.status.success(), "{}", text(&inspected));
        let out = text(&inspected);
        assert!(out.contains("project") && out.contains("stdio"), "{out}");
        assert!(
            !out.contains(secret) && !out.contains("example.invalid"),
            "{out}"
        );
    }
    let failed = run(root, &["mcp", "ping", "remote", "-s", "project"]);
    assert!(!failed.status.success());
    assert!(!text(&failed).contains(secret), "{}", text(&failed));
    let bad_url = format!("not-a-url-{secret}");
    let bad = serde_json::json!({"type":"sse", "url":bad_url}).to_string();
    assert!(
        run(root, &["mcp", "add-json", "bad", &bad, "-s", "user"])
            .status
            .success()
    );
    let failed = run(root, &["mcp", "ping", "bad", "-s", "user"]);
    assert!(!failed.status.success());
    assert!(!text(&failed).contains(secret), "{}", text(&failed));
    assert!(
        run(root, &["mcp", "remove", "remote", "-s", "project"])
            .status
            .success()
    );
    let server = serde_json::json!({"command":env!("CARGO_BIN_EXE_mock_mcp_server")}).to_string();
    assert!(
        run(
            root,
            &["mcp", "add-json", "remote", &server, "-s", "project"]
        )
        .status
        .success()
    );
    // A permissive diagnostic must not claim success for a file chat rejects.
    let malformed = serde_json::json!({
        "command":env!("CARGO_BIN_EXE_mock_mcp_server"), "args":[7, secret]
    })
    .to_string();
    assert!(
        run(root, &["mcp", "add-json", "malformed", &malformed])
            .status
            .success()
    );
    for action in ["ping", "test", "get"] {
        let rejected = run(root, &["mcp", action, "malformed"]);
        assert!(!rejected.status.success(), "{action}: {}", text(&rejected));
        assert!(
            text(&rejected).contains("Failed to parse"),
            "{}",
            text(&rejected)
        );
        assert!(!text(&rejected).contains(secret), "{}", text(&rejected));
    }
    assert!(run(root, &["mcp", "remove", "malformed"]).status.success());

    // A disabled project entry must not execute or fall back to the user entry.
    let disabled = serde_json::json!({
        "command":env!("CARGO_BIN_EXE_mock_mcp_server"), "disabled":true
    })
    .to_string();
    assert!(
        run(
            root,
            &["mcp", "add-json", "scoped-check", &server, "-s", "user"]
        )
        .status
        .success()
    );
    assert!(
        run(root, &["mcp", "add-json", "scoped-check", &disabled])
            .status
            .success()
    );
    let effective = run(root, &["mcp", "get", "scoped-check"]);
    assert!(effective.status.success() && text(&effective).contains("Scope: user"));
    for action in ["ping", "test"] {
        let rejected = run(root, &["mcp", action, "scoped-check", "-s", "project"]);
        assert!(!rejected.status.success(), "{action}: {}", text(&rejected));
        assert!(
            text(&rejected).contains("No enabled MCP server"),
            "{}",
            text(&rejected)
        );
    }
    assert!(
        run(root, &["mcp", "ping", "scoped-check", "-s", "user"])
            .status
            .success()
    );
    let user_path = root.join("user/mcp.json");
    let prior: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&user_path).unwrap()).unwrap();
    for index in (0..20).filter(|index| index % 3 == 0) {
        let name = format!("concurrent-{index}");
        let seed = run(
            root,
            &["mcp", "add", "-s", "user", &name, "fixture-not-executed"],
        );
        assert!(seed.status.success(), "{}", text(&seed));
    }
    let mut children = Vec::new();
    for index in 0..20 {
        let name = format!("concurrent-{index}");
        let entry = r#"{"command":"fixture-not-executed"}"#;
        let args = if index % 3 == 0 {
            vec!["mcp", "remove", name.as_str(), "-s", "user"]
        } else if index % 3 == 1 {
            vec!["mcp", "add-json", name.as_str(), entry, "-s", "user"]
        } else {
            vec![
                "mcp",
                "add",
                "-s",
                "user",
                name.as_str(),
                "fixture-not-executed",
            ]
        };
        children.push(
            command(root, &args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", text(&output));
    }
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&user_path).unwrap()).unwrap();
    let entries = config["mcpServers"].as_object().unwrap();
    assert_eq!(
        entries.len(),
        prior["mcpServers"].as_object().unwrap().len()
            + (0..20).filter(|index| index % 3 != 0).count()
    );
    for (name, entry) in prior["mcpServers"].as_object().unwrap() {
        assert_eq!(entries.get(name), Some(entry));
    }
    for index in 0..20 {
        let name = format!("concurrent-{index}");
        if index % 3 == 0 {
            assert!(!entries.contains_key(&name));
        } else {
            assert_eq!(entries[&name]["command"], "fixture-not-executed");
        }
    }

    // Only this explicitly requested connectivity check launches the fixture server.
    let ping = run(root, &["mcp", "ping", "remote", "-s", "project"]);
    assert!(ping.status.success(), "{}", text(&ping));
}
