use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn read_workspace_file(relative: &str) -> String {
    let path = workspace_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn workspace_path(relative: &str) -> PathBuf {
    workspace_root().join(relative)
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| {
            dir.join("Cargo.toml").is_file()
                && dir.join("crates").is_dir()
                && dir.join("Dockerfile").is_file()
        })
        .unwrap_or_else(|| {
            panic!(
                "could not find workspace root from {}",
                env!("CARGO_MANIFEST_DIR")
            )
        })
        .to_path_buf()
}

#[test]
fn dockerfile_builds_from_workspace_root() {
    let dockerfile = read_workspace_file("Dockerfile");

    for forbidden in [
        "WORKDIR /app/rust",
        "COPY rust/",
        "COPY --from=planner /app/rust/recipe.json",
    ] {
        assert!(
            !dockerfile.contains(forbidden),
            "Dockerfile must not reference the removed rust/ workspace path: {forbidden}"
        );
    }

    assert!(
        dockerfile.contains("WORKDIR /app"),
        "Dockerfile must build from the repository root workspace"
    );

    let (_, planner_and_later) = dockerfile
        .split_once("FROM chef AS planner")
        .expect("Dockerfile must define the cargo-chef planner stage");
    let (planner, builder) = planner_and_later
        .split_once("FROM chef AS builder")
        .expect("Dockerfile must define the cargo-chef builder stage");

    for (name, stage) in [("planner", planner), ("builder", builder)] {
        assert!(
            stage.contains("COPY Cargo.toml Cargo.lock ./"),
            "{name} stage must copy the root workspace manifests"
        );
        assert!(
            stage.contains("COPY crates ./crates"),
            "{name} stage must copy the root workspace crates"
        );
    }
    assert!(
        !dockerfile.contains("COPY . ./"),
        "Dockerfile must use scoped workspace copies so unrelated files do not invalidate Rust layers"
    );
    assert!(
        dockerfile.contains("COPY --from=planner /app/recipe.json recipe.json"),
        "builder stage must read cargo-chef recipe from the root workspace"
    );
}

#[test]
fn dockerignore_excludes_only_the_whole_removed_workspace_path() {
    let dockerignore = read_workspace_file(".dockerignore");
    let mut seen = HashSet::new();
    let mut ignores_removed_workspace_dir = false;

    for raw_line in dockerignore.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with("rust/") {
            assert_eq!(
                line, "rust/",
                ".dockerignore must ignore only the whole removed rust/ workspace path, not stale subpath rules"
            );
            ignores_removed_workspace_dir = true;
        }
        assert!(
            seen.insert(line.to_string()),
            ".dockerignore must not contain duplicate ignore entries: {line}"
        );
    }

    assert!(
        ignores_removed_workspace_dir,
        ".dockerignore must exclude untracked local rust/ residues from Docker build context"
    );
}

#[test]
fn developer_guidance_uses_repo_root_workspace() {
    for relative in [
        "AGENTS.md",
        "CLAUDE.md",
        ".claude/CLAUDE.md",
        ".cursor/rules/project-rules.mdc",
        ".kiro/steering/project-rules.md",
        ".agent/skills/astra-dev/SKILL.md",
        ".agent/skills/verify_task/SKILL.md",
        ".claude/skills/astra-dev/SKILL.md",
        ".claude/skills/verify_task/SKILL.md",
        "docs/testing/system-e2e-matrix.md",
    ] {
        let text = read_workspace_file(relative);
        for forbidden in [
            "cd rust",
            "crates under rust/",
            "from `rust/`",
            "under `rust/`",
            "rust/             # Cargo workspace",
            "Cargo Workspace Lives Under `rust/`",
            "Cargo workspace lives under `rust/`",
            "no Cargo.toml at repo root",
            "/astra/rust",
        ] {
            assert!(
                !text.contains(forbidden),
                "{relative} still references the removed rust/ workspace layout: {forbidden}"
            );
        }
    }
}

#[test]
fn developer_tool_adapters_delegate_to_canonical_guidance() {
    for relative in [
        "CLAUDE.md",
        ".claude/CLAUDE.md",
        ".cursor/rules/project-rules.mdc",
        ".kiro/steering/project-rules.md",
    ] {
        let text = read_workspace_file(relative);
        assert!(
            text.contains("AGENTS.md"),
            "{relative} must delegate repository-wide rules to AGENTS.md"
        );
        assert!(
            text.contains("canonical"),
            "{relative} must identify AGENTS.md as the canonical guidance"
        );
    }
}

#[test]
fn design_index_covers_every_design_document() {
    let design_root = workspace_path("docs/design");
    let index = read_workspace_file("docs/design/README.md");

    for entry in std::fs::read_dir(&design_root)
        .unwrap_or_else(|e| panic!("read {}: {e}", design_root.display()))
    {
        let entry = entry.unwrap_or_else(|e| panic!("read design entry: {e}"));
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .expect("design document file name must be UTF-8");
        if name == "README.md" {
            continue;
        }
        assert!(
            index.contains(&format!("]({name})")),
            "docs/design/{name} must appear in the canonical design index"
        );
    }
}
