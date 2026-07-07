use std::path::{Path, PathBuf};

pub fn workspace_root() -> PathBuf {
    workspace_root_from(env!("CARGO_MANIFEST_DIR"))
}

pub fn workspace_path(relative: impl AsRef<Path>) -> PathBuf {
    workspace_root().join(relative)
}

pub fn workspace_root_from(start: impl AsRef<Path>) -> PathBuf {
    try_workspace_root_from(start.as_ref()).unwrap_or_else(|| {
        panic!(
            "could not find Astra workspace root from {}",
            start.as_ref().display()
        )
    })
}

pub fn try_workspace_root_from(start: &Path) -> Option<PathBuf> {
    let start = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    start
        .ancestors()
        .find(|dir| is_astra_workspace_root(dir))
        .map(Path::to_path_buf)
}

fn is_astra_workspace_root(dir: &Path) -> bool {
    let Ok(manifest) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return false;
    };
    manifest.lines().any(|line| line.trim() == "[workspace]")
        && dir.join("crates").is_dir()
        && dir.join("fixtures").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_workspace_root_from_core_manifest_dir() {
        let root = workspace_root_from(env!("CARGO_MANIFEST_DIR"));
        assert!(root.join("Cargo.toml").is_file());
        assert!(root.join("crates/core").is_dir());
    }

    #[test]
    fn finds_workspace_root_from_nested_file_path() {
        let root = workspace_root();
        let nested = root.join("crates/core/src/config.rs");
        assert_eq!(workspace_root_from(&nested), root);
    }
}
