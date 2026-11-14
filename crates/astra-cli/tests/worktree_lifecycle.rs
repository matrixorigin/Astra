//! Exercise Worktree through the same early process-supervisor entrypoint as Astra.
//!
//! A libtest process cannot re-exec that entrypoint, so this executable owns
//! one nextest-compatible test and can also serve as its own supervisor.

use astra_cli::edge_tools::ToolExecutor;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Some(code) = astra_sandbox::run_invocation_supervisor_if_requested() {
        std::process::exit(code);
    }
    let args: Vec<_> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            println!("worktree_lifecycle: test");
        }
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    // SAFETY: this dedicated process is single-threaded before the runtime starts.
    unsafe {
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_GLOBAL", temp.path().join("no-global-gitconfig"));
        std::env::set_var("GIT_TERMINAL_PROMPT", "0");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(worktree_lifecycle(temp.path()));
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn init_repo(root: &Path) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Worktree Test"]);
    git(root, &["config", "user.email", "worktree@example.invalid"]);
    std::fs::write(root.join("tracked.txt"), "committed\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "initial"]);
}

fn add_worktree(repo: &Path, parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    git(
        repo,
        &["worktree", "add", "-qb", name, path.to_str().unwrap()],
    );
    path
}

async fn enter(repo: &Path, branch: &str) -> ToolExecutor {
    let executor = ToolExecutor::new(repo);
    executor.set_current_visible_tool_schemas(&astra_tools::schemas::all_tool_schemas());
    let result = executor
        .execute_with_metadata("worktree", &json!({"action":"enter", "branch":branch}))
        .await;
    assert!(!result.is_error, "{result:?}");
    executor
}

async fn remove(
    executor: &ToolExecutor,
    discard: bool,
) -> astra_tools::execution_outcome::ToolExecutionOutcome {
    executor
        .execute_with_metadata(
            "worktree",
            &json!({"action":"exit", "exit_action":"remove", "discard_changes":discard}),
        )
        .await
}

async fn worktree_lifecycle(temp: &Path) {
    let repo = temp.join("repo");
    init_repo(&repo);

    let clean = add_worktree(&repo, temp, "clean");
    let executor = enter(&repo, "clean").await;
    if cfg!(target_os = "macos") {
        let refused = remove(&executor, true).await;
        assert!(
            refused.is_error && refused.output.contains("unavailable on macOS"),
            "{refused:?}"
        );
        assert!(clean.exists() && executor.in_worktree_session());
        let kept = executor
            .execute_with_metadata("worktree", &json!({"action":"exit", "exit_action":"keep"}))
            .await;
        assert!(!kept.is_error, "{kept:?}");
        git(
            &repo,
            &["worktree", "remove", "--force", clean.to_str().unwrap()],
        );
        return;
    }
    if !cfg!(target_os = "linux") {
        return;
    }

    let removed = remove(&executor, false).await;
    assert!(!removed.is_error, "{removed:?}");
    assert!(!clean.exists() && !executor.in_worktree_session());
    git(&repo, &["show-ref", "--verify", "refs/heads/clean"]);

    for (name, flag) in [
        ("assume", "--assume-unchanged"),
        ("sparse", "--skip-worktree"),
    ] {
        let checkout = add_worktree(&repo, temp, name);
        git(&checkout, &["update-index", flag, "tracked.txt"]);
        std::fs::write(checkout.join("tracked.txt"), "uncommitted work\n").unwrap();
        let executor = enter(&repo, name).await;
        let refused = remove(&executor, false).await;
        assert!(
            refused.is_error && executor.in_worktree_session(),
            "{refused:?}"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("tracked.txt")).unwrap(),
            "uncommitted work\n"
        );
        let removed = remove(&executor, true).await;
        assert!(!removed.is_error && !checkout.exists(), "{removed:?}");
        git(
            &repo,
            &["show-ref", "--verify", &format!("refs/heads/{name}")],
        );
    }

    let subrepo = temp.join("subrepo");
    init_repo(&subrepo);
    std::fs::write(subrepo.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&subrepo, &["add", ".gitignore"]);
    git(&subrepo, &["commit", "-qm", "ignore local files"]);
    git(
        &repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            subrepo.to_str().unwrap(),
            "module",
        ],
    );
    git(&repo, &["commit", "-qm", "add submodule"]);
    git(&repo, &["config", "submodule.module.ignore", "all"]);
    let checkout = add_worktree(&repo, temp, "submodule");
    git(
        &checkout,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "-q",
        ],
    );
    std::fs::write(checkout.join("module/untracked.txt"), "untracked work\n").unwrap();
    std::fs::write(checkout.join("module/ignored.txt"), "ignored work\n").unwrap();
    let executor = enter(&repo, "submodule").await;
    let refused = remove(&executor, false).await;
    assert!(
        refused.is_error && executor.in_worktree_session(),
        "{refused:?}"
    );
    assert!(checkout.join("module/untracked.txt").exists());
    std::fs::remove_file(checkout.join("module/untracked.txt")).unwrap();
    let refused = remove(&executor, false).await;
    assert!(
        refused.is_error && executor.in_worktree_session(),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(checkout.join("module/ignored.txt")).unwrap(),
        "ignored work\n"
    );
    let removed = remove(&executor, true).await;
    assert!(!removed.is_error && !checkout.exists(), "{removed:?}");
    git(&repo, &["show-ref", "--verify", "refs/heads/submodule"]);

    let checkout = add_worktree(&repo, temp, "detached");
    let executor = enter(&repo, "detached").await;
    git(&checkout, &["checkout", "--detach", "-q"]);
    std::fs::write(checkout.join("tracked.txt"), "detached commit\n").unwrap();
    git(&checkout, &["commit", "-qam", "detached commit"]);
    let refused = remove(&executor, false).await;
    assert!(
        refused.is_error && executor.in_worktree_session(),
        "{refused:?}"
    );
    assert!(checkout.exists());
    let removed = remove(&executor, true).await;
    assert!(!removed.is_error && !checkout.exists(), "{removed:?}");
}
