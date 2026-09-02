use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUILD_ATTESTATION_NONCE_ENV: &str = "ASTRA_BUILD_ATTESTATION_NONCE";
const BUILD_SOURCE_GIT_SHA_ENV: &str = "ASTRA_BUILD_SOURCE_GIT_SHA";
const BUILD_SOURCE_GIT_DIRTY_ENV: &str = "ASTRA_BUILD_SOURCE_GIT_DIRTY";

fn valid_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_text(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|text| text.trim().to_string())
}

fn git_path(workspace: &Path, name: &str) -> Option<PathBuf> {
    let raw = git_text(workspace, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    })
}

fn watch_git_path(workspace: &Path, name: &str) {
    if let Some(path) = git_path(workspace, name) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed={BUILD_ATTESTATION_NONCE_ENV}");
    println!("cargo:rerun-if-env-changed={BUILD_SOURCE_GIT_SHA_ENV}");
    println!("cargo:rerun-if-env-changed={BUILD_SOURCE_GIT_DIRTY_ENV}");
    let build_target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    let build_profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let workspace = manifest
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| manifest.join("../.."));

    // Cargo otherwise has no reason to rerun this script after a commit when
    // Rust sources are unchanged. Watch both detached-HEAD and symbolic-ref
    // forms, including packed refs and worktree layouts.
    watch_git_path(&workspace, "HEAD");
    watch_git_path(&workspace, "index");
    watch_git_path(&workspace, "logs/HEAD");
    watch_git_path(&workspace, "packed-refs");
    if let Some(symbolic_ref) = git_text(&workspace, &["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&workspace, &symbolic_ref);
    }

    // Container builds intentionally exclude `.git` from their context. Their
    // trusted build orchestrator passes the same source identity used for OCI
    // image metadata so the binary and image remain auditable as one artifact.
    let git_sha = env::var(BUILD_SOURCE_GIT_SHA_ENV)
        .ok()
        .filter(|sha| valid_git_sha(sha))
        .or_else(|| {
            git_text(&workspace, &["rev-parse", "--verify", "HEAD^{commit}"])
                .filter(|sha| valid_git_sha(sha))
        })
        .unwrap_or_else(|| "unknown".to_string());
    // Executable identity gates only on tracked source changes.  Benchmark
    // artifacts, logs, and user worktrees are intentionally untracked and
    // must not make an otherwise reproducible build unverifiable.
    let git_dirty = match env::var(BUILD_SOURCE_GIT_DIRTY_ENV).as_deref() {
        Ok("true") => true,
        Ok("false") => false,
        _ => git_text(
            &workspace,
            &["status", "--porcelain=v1", "--untracked-files=no"],
        )
        .map(|status| !status.is_empty())
        .unwrap_or(true),
    };
    let attestation_nonce = env::var(BUILD_ATTESTATION_NONCE_ENV)
        .ok()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "absent".to_string());
    println!("cargo:rustc-env=ASTRA_BUILD_GIT_SHA={git_sha}");
    println!("cargo:rustc-env=ASTRA_BUILD_GIT_DIRTY={git_dirty}");
    println!("cargo:rustc-env=ASTRA_BUILD_ATTESTATION_NONCE={attestation_nonce}");
    println!("cargo:rustc-env=ASTRA_BUILD_TARGET={build_target}");
    println!("cargo:rustc-env=ASTRA_BUILD_PROFILE={build_profile}");
}
