//! P0-T end-to-end: user's `runtime.toml` `[tool_surface].always_load_tools`
//! flows all the way through `RuntimeConfig::load` → `ToolSurface::build`
//! → always_load schemas that would be sent on the wire as `tools[]`.
//!
//! Without this test, a typo in any hop (config → surface → wire) fails
//! silently. The whole "user can customize their tools[]" promise rests
//! on this path being intact.

use astra_config::runtime_config::RuntimeConfig;
use astra_runtime::tool_registry::surface::ToolSurface;
use serde_json::Value;
use std::io::Write;

fn catalog_schemas() -> Vec<Value> {
    let mut schemas = astra_tools::schemas::all_tool_schemas();
    schemas.push(astra_runtime::turn::skill_tool::skill_tool_schema_v2());
    schemas
}

fn names(schemas: &[Value]) -> Vec<String> {
    schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .collect()
}

/// Run a test block with a temporary `~/.astra/config/runtime.toml`.
/// Restores the original HOME on drop.
fn with_user_runtime_toml<F: FnOnce(&RuntimeConfig)>(contents: &str, f: F) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let astra_dir = tmp.path().join(".astra").join("config");
    std::fs::create_dir_all(&astra_dir).unwrap();
    let toml_path = astra_dir.join("runtime.toml");
    let mut file = std::fs::File::create(&toml_path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    drop(file);

    // Scoped HOME override. RuntimeConfig::load reads dirs::home_dir() →
    // $HOME on Linux. Previous value restored after test.
    // Tests in this file run serially via `serial_test` to avoid races.
    let old_home = std::env::var("HOME").ok();
    // SAFETY: serial_test gates ensure no concurrent test reads HOME.
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }

    let config = RuntimeConfig::load();
    f(&config);

    unsafe {
        if let Some(h) = old_home {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
    }
}

#[test]
#[serial_test::serial]
fn user_always_load_tools_with_dash_removes_default_from_wire() {
    with_user_runtime_toml(
        r#"
[tool_surface]
always_load_tools = ["-grep"]
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let always_load = names(&surface.always_load_schemas());
            assert!(
                !always_load.iter().any(|n| n == "grep"),
                "user TOML said `-grep` — grep must NOT be in wire tools[]: got {always_load:?}"
            );
            // grep now lives in deferred.
            let deferred: Vec<&str> = surface.deferred().iter().map(|e| e.name.as_str()).collect();
            assert!(
                deferred.contains(&"grep"),
                "grep must appear in deferred list"
            );
        },
    );
}

#[test]
#[serial_test::serial]
fn user_always_load_tools_adds_github_to_wire() {
    with_user_runtime_toml(
        r#"
[tool_surface]
always_load_tools = ["github"]
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let always_load = names(&surface.always_load_schemas());
            assert!(
                always_load.iter().any(|n| n == "github"),
                "github must be always_load per user config: got {always_load:?}"
            );
            // Default always-load tools still there.
            assert!(always_load.iter().any(|n| n == "bash"));
            assert!(always_load.iter().any(|n| n == "memory"));
        },
    );
}

#[test]
#[serial_test::serial]
fn missing_toml_defaults_are_in_wire() {
    // Empty TOML dir — no overrides. Wire reflects the default always_load identities exactly.
    with_user_runtime_toml("# empty", |config| {
        let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
        let always_load = names(&surface.always_load_schemas());
        // Spot check: all defaults present, no extras.
        for must in [
            "ask_user",
            "bash",
            "read_file",
            "git",
            "grep",
            "memory",
            "skill",
            "tool_search",
        ] {
            assert!(
                always_load.iter().any(|n| n == must),
                "missing default {must}"
            );
        }
        assert!(!always_load.iter().any(|n| n == "github"));
        assert!(!always_load.iter().any(|n| n == "web_fetch"));
    });
}

#[test]
#[serial_test::serial]
fn malformed_toml_falls_back_to_defaults_silently() {
    // Malformed TOML: the load() path silently uses defaults. Document
    // this behavior with an explicit test so it doesn't regress into
    // either (a) panic mid-turn or (b) random hot-reload behavior.
    with_user_runtime_toml(
        r#"
[tool_surface
always_load_tools = ["github
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let always_load = names(&surface.always_load_schemas());
            assert!(
                !always_load.iter().any(|n| n == "github"),
                "malformed TOML must NOT silently always-load github; fallback to defaults"
            );
            assert!(always_load.iter().any(|n| n == "bash"));
        },
    );
}
