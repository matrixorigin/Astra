//! P0-T end-to-end: user's `runtime.toml` `[tool_surface].pinned_tools`
//! flows all the way through `RuntimeConfig::load` → `ToolSurface::build`
//! → pinned schemas that would be sent on the wire as `tools[]`.
//!
//! Without this test, a typo in any hop (config → surface → wire) fails
//! silently. The whole "user can customize their tools[]" promise rests
//! on this path being intact.

use astra_config::runtime_config::RuntimeConfig;
use astra_runtime::tool_registry::surface::ToolSurface;
use astra_turn_core::tool_registry_meta::TOOL_CATALOG;
use serde_json::{Value, json};
use std::io::Write;

fn catalog_schemas() -> Vec<Value> {
    let mut out: Vec<Value> = TOOL_CATALOG
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        })
        .collect();
    for name in ["skill", "tool_search"] {
        out.push(json!({
            "type": "function",
            "function": {
                "name": name,
                "description": "",
                "parameters": {"type": "object", "properties": {}}
            }
        }));
    }
    out
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
fn user_pinned_tools_with_dash_removes_default_from_wire() {
    with_user_runtime_toml(
        r#"
[tool_surface]
pinned_tools = ["-grep"]
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let pinned = names(&surface.pinned_schemas());
            assert!(
                !pinned.iter().any(|n| n == "grep"),
                "user TOML said `-grep` — grep must NOT be in wire tools[]: got {pinned:?}"
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
fn user_pinned_tools_adds_github_to_wire() {
    with_user_runtime_toml(
        r#"
[tool_surface]
pinned_tools = ["github"]
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let pinned = names(&surface.pinned_schemas());
            assert!(
                pinned.iter().any(|n| n == "github"),
                "github must be pinned per user config: got {pinned:?}"
            );
            // Default pins still there.
            assert!(pinned.iter().any(|n| n == "bash"));
            assert!(pinned.iter().any(|n| n == "memory"));
        },
    );
}

#[test]
#[serial_test::serial]
fn missing_toml_defaults_are_in_wire() {
    // Empty TOML dir — no overrides. Wire reflects DEFAULT_PINNED exactly.
    with_user_runtime_toml("# empty", |config| {
        let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
        let pinned = names(&surface.pinned_schemas());
        // Spot check: all defaults present, no extras.
        for must in [
            "bash",
            "read_file",
            "grep",
            "memory",
            "skill",
            "tool_search",
        ] {
            assert!(pinned.iter().any(|n| n == must), "missing default {must}");
        }
        assert!(!pinned.iter().any(|n| n == "github"));
        assert!(!pinned.iter().any(|n| n == "web_fetch"));
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
pinned_tools = ["github
"#,
        |config| {
            let surface = ToolSurface::build(catalog_schemas(), &config.tool_surface, &[]);
            let pinned = names(&surface.pinned_schemas());
            assert!(
                !pinned.iter().any(|n| n == "github"),
                "malformed TOML must NOT silently pin github; fallback to defaults"
            );
            assert!(pinned.iter().any(|n| n == "bash"));
        },
    );
}
