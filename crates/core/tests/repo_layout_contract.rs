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
    assert!(
        dockerfile.contains("COPY . ./"),
        "Dockerfile must copy the root workspace into cargo-chef stages"
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
        "CLAUDE.md",
        ".claude/CLAUDE.md",
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
