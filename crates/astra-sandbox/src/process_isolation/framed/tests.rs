use super::*;
use crate::CgroupGuard;

fn limits() -> FramedProcessLimits {
    FramedProcessLimits {
        max_frame_bytes: 64,
        max_queued_frames: 1,
        max_stderr_bytes: 16,
        timeout: Duration::from_secs(5),
    }
}

#[tokio::test]
async fn weak_ownership_and_cancelled_admission_never_spawn() {
    let directory = tempfile::tempdir().unwrap();
    let command = || {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "touch started"])
            .current_dir(directory.path());
        command
    };
    for cancelled in [false, true] {
        let owner = BashInvocationOwner {
            process_scope: CgroupGuard {
                cg_path: None,
                procs_path: None,
            },
            supervisor: None,
        };
        let cancel = CancellationToken::new();
        if cancelled {
            cancel.cancel();
        }
        let error = owner
            .spawn_framed(command(), limits(), cancel)
            .err()
            .unwrap();
        assert_eq!(
            error.kind(),
            if cancelled {
                io::ErrorKind::Interrupted
            } else {
                io::ErrorKind::Unsupported
            }
        );
        assert!(!directory.path().join("started").exists());
    }
}

#[test]
fn invalid_and_overflowing_budgets_are_rejected() {
    for budget in [
        FramedProcessLimits {
            max_frame_bytes: 0,
            ..limits()
        },
        FramedProcessLimits {
            max_queued_frames: 0,
            ..limits()
        },
        FramedProcessLimits {
            max_queued_frames: usize::MAX,
            ..limits()
        },
        FramedProcessLimits {
            max_stderr_bytes: usize::MAX,
            ..limits()
        },
        FramedProcessLimits {
            timeout: Duration::ZERO,
            ..limits()
        },
    ] {
        assert_eq!(
            budget.deadline().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn spawn(
        script: &str,
        directory: &std::path::Path,
        budget: FramedProcessLimits,
        cancel: CancellationToken,
    ) -> FramedProcess {
        spawn_with_helper(
            script,
            directory,
            budget,
            cancel,
            "process_isolation::tests::invocation_supervisor_test_helper",
        )
    }

    fn spawn_with_helper(
        script: &str,
        directory: &std::path::Path,
        budget: FramedProcessLimits,
        cancel: CancellationToken,
        helper: &str,
    ) -> FramedProcess {
        let (mut command, owner) = BashInvocationOwner::prepare_with_supervisor_helper(
            std::env::current_exe().unwrap(),
            [
                "--exact".into(),
                helper.into(),
                "--nocapture".into(),
                "--quiet".into(),
            ],
            "/bin/sh",
            &["-c".into(), script.into()],
        )
        .unwrap();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(directory);
        owner.spawn_framed(command, budget, cancel).unwrap()
    }

    // The re-exec test harness prints a prelude before entering the existing
    // supervisor helper. Only tests skip that exact prelude; production never
    // interprets payload text or strips non-JSON output from a native process.
    async fn next_payload(process: &mut FramedProcess) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = process
                    .recv_frame()
                    .await
                    .expect("payload before stdout EOF");
                if frame.is_empty() || frame == b"running 1 test" {
                    continue;
                }
                return frame;
            }
        })
        .await
        .unwrap()
    }

    fn assert_authority(outcome: &FramedProcessOutcome) {
        assert_eq!(outcome.target_released, Some(true));
        assert_eq!(
            outcome.settlement.unwrap().ownership,
            super::super::super::ScopeOwnership::InvocationSupervisor
        );
    }

    async fn wait_ready(directory: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !directory.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    const BLOCKED_HELPER: &str =
        "process_isolation::framed::tests::linux::blocked_ready_supervisor_helper";

    /// A real helper held before READY by a deterministic test-only barrier.
    /// Ordinary execution of this test is a no-op, without any env mutation.
    #[test]
    fn blocked_ready_supervisor_helper() {
        if !crate::invocation_supervisor_is_requested() {
            return;
        }
        std::fs::write("ready", b"helper entered").unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !std::path::Path::new("release-helper").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "test barrier not released"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(code) = crate::run_invocation_supervisor_if_requested() {
            std::process::exit(code);
        }
    }

    #[tokio::test]
    async fn cancellation_before_driver_handshake_never_releases_target() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        // Current-thread Tokio cannot poll the newly spawned driver before
        // cancellation: there is intentionally no await between these calls.
        let process = spawn("touch started", directory.path(), limits(), token.clone());
        token.cancel();
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Cancelled),
            "{outcome:?}"
        );
        assert_eq!(outcome.target_released, Some(false));
        assert!(
            outcome.settlement.is_none(),
            "pre-START exit is not an ECHILD receipt"
        );
        assert!(!directory.path().join("started").exists());
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_ready_never_sends_start() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let process = spawn_with_helper(
            "touch started",
            directory.path(),
            limits(),
            token.clone(),
            BLOCKED_HELPER,
        );
        wait_ready(directory.path()).await;
        token.cancel();
        std::fs::write(
            directory.path().join("release-helper"),
            b"release after cancel",
        )
        .unwrap();
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Cancelled),
            "{outcome:?}"
        );
        assert_eq!(outcome.target_released, Some(false));
        assert!(outcome.settlement.is_none());
        assert!(!directory.path().join("started").exists());
    }

    #[tokio::test]
    async fn startup_deadline_waits_for_owned_cleanup_without_releasing_target() {
        let directory = tempfile::tempdir().unwrap();
        let budget = FramedProcessLimits {
            timeout: Duration::from_millis(50),
            ..limits()
        };
        let process = spawn_with_helper(
            "touch started",
            directory.path(),
            budget,
            CancellationToken::new(),
            BLOCKED_HELPER,
        );
        wait_ready(directory.path()).await;
        tokio::time::sleep(budget.timeout + Duration::from_millis(10)).await;
        std::fs::write(
            directory.path().join("release-helper"),
            b"release after deadline",
        )
        .unwrap();
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::TimedOut),
            "{outcome:?}"
        );
        assert_eq!(outcome.target_released, Some(false));
        assert!(outcome.settlement.is_none());
        assert!(outcome.status.is_some(), "cleanup must await the helper");
        assert!(!directory.path().join("started").exists());
    }

    #[tokio::test]
    async fn dropped_transport_before_handshake_keeps_cleanup_owned() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let process = spawn("touch started", directory.path(), limits(), token.clone());
        let input = process.input();
        drop(process);
        tokio::time::timeout(Duration::from_secs(5), input.sender.closed())
            .await
            .unwrap();
        assert!(!token.is_cancelled());
        assert!(!directory.path().join("started").exists());
    }

    fn escaped_pid_path(directory: &std::path::Path) -> std::path::PathBuf {
        let pid = std::fs::read_to_string(directory.join("escaped-pid"))
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        std::path::PathBuf::from(format!("/proc/{pid}"))
    }

    #[tokio::test]
    async fn bidirectional_exact_limit_frames_preserve_nonzero_exit_and_cap_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let mut process = spawn(
            "IFS= read -r first; printf '%s\\n' \"$first\"; printf '%0200000d' 0 >&2; IFS= read -r second; printf '%s\\n' \"$second\"; exit 7",
            directory.path(),
            limits(),
            CancellationToken::new(),
        );
        let input = process.input();
        let first = vec![b'x'; 64];
        input.send_frame(&first).await.unwrap();
        assert_eq!(next_payload(&mut process).await, first);
        input.send_frame(br#"{"second":true}"#).await.unwrap();
        assert_eq!(next_payload(&mut process).await, br#"{"second":true}"#);
        drop(input);
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Exited),
            "{outcome:?}"
        );
        assert_eq!(outcome.status.unwrap().code(), Some(7));
        assert_eq!(outcome.stderr, vec![b'0'; 16]);
        assert!(outcome.stderr_capped);
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn invalid_input_is_not_written_and_does_not_poison_next_frame() {
        let directory = tempfile::tempdir().unwrap();
        let mut process = spawn(
            "IFS= read -r frame; printf '%s\\n' \"$frame\"",
            directory.path(),
            limits(),
            CancellationToken::new(),
        );
        let input = process.input();
        for invalid in [
            vec![b'x'; 65],
            b"{}\n{}".to_vec(),
            b"{}\r{}".to_vec(),
            Vec::new(),
        ] {
            assert_eq!(
                input.send_frame(&invalid).await.unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        input.send_frame(br#"{"valid":true}"#).await.unwrap();
        assert_eq!(next_payload(&mut process).await, br#"{"valid":true}"#);
        drop(input);
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Exited),
            "{outcome:?}"
        );
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn oversized_unterminated_stdout_settles_escaped_descendants() {
        let directory = tempfile::tempdir().unwrap();
        let process = spawn(
            "setsid /bin/sh -c 'echo $$ > escaped-pid; touch ready; sleep 10; printf escaped > late' </dev/null >/dev/null 2>&1 & while [ ! -f ready ]; do sleep 0.005; done; printf '%070d' 0; sleep 10",
            directory.path(),
            limits(),
            CancellationToken::new(),
        );
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(&outcome.end, FramedProcessEnd::OutputFailed(error) if error.kind() == io::ErrorKind::InvalidData),
            "{outcome:?}"
        );
        assert_authority(&outcome);
        assert!(outcome.settlement.unwrap().descendants_terminated);
        assert!(!escaped_pid_path(directory.path()).exists());
        assert!(!directory.path().join("late").exists());
    }

    #[tokio::test]
    async fn eof_never_promotes_a_partial_frame_to_a_complete_payload() {
        let directory = tempfile::tempdir().unwrap();
        let process = spawn(
            "printf '{\"partial\":'",
            directory.path(),
            limits(),
            CancellationToken::new(),
        );
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(&outcome.end, FramedProcessEnd::OutputFailed(error) if error.kind() == io::ErrorKind::UnexpectedEof),
            "{outcome:?}"
        );
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn blocked_stdin_and_full_input_queue_are_cancelled_and_settled() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let budget = FramedProcessLimits {
            max_frame_bytes: 1024 * 1024,
            ..limits()
        };
        let process = spawn(
            "touch ready; sleep 10",
            directory.path(),
            budget,
            token.clone(),
        );
        wait_ready(directory.path()).await;
        let input = process.input();
        let frame = vec![b'x'; budget.max_frame_bytes];
        input.send_frame(&frame).await.unwrap();
        input.send_frame(&frame).await.unwrap();
        let blocked = input.send_frame(&frame);
        tokio::pin!(blocked);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut blocked)
                .await
                .is_err()
        );
        token.cancel();
        assert_eq!(
            blocked.await.unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Cancelled),
            "{outcome:?}"
        );
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn unread_stdout_backpressure_does_not_block_cancel_or_descendant_settlement() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let process = spawn(
            "setsid /bin/sh -c 'echo $$ > escaped-pid; touch ready; sleep 10; printf escaped > late' </dev/null >/dev/null 2>&1 & while :; do printf '{\"tick\":true}\\n'; done",
            directory.path(),
            limits(),
            token.clone(),
        );
        wait_ready(directory.path()).await;
        // Leave stdout unread: the helper prelude already fills the one-slot queue.
        token.cancel();
        let outcome = process.wait().await.unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::Cancelled),
            "{outcome:?}"
        );
        assert_authority(&outcome);
        assert!(!escaped_pid_path(directory.path()).exists());
        assert!(!directory.path().join("late").exists());
    }

    #[tokio::test]
    async fn deadline_terminates_a_process_with_blocked_output() {
        let directory = tempfile::tempdir().unwrap();
        let budget = FramedProcessLimits {
            timeout: Duration::from_secs(3),
            ..limits()
        };
        let process = spawn(
            "touch ready; sleep 10",
            directory.path(),
            budget,
            CancellationToken::new(),
        );
        wait_ready(directory.path()).await;
        let outcome = tokio::time::timeout(Duration::from_secs(6), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(outcome.end, FramedProcessEnd::TimedOut),
            "{outcome:?}"
        );
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn dropping_wait_future_cancels_private_driver_without_cancelling_parent() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let process = spawn(
            "setsid /bin/sh -c 'echo $$ > escaped-pid; touch ready; sleep 10; printf escaped > late' </dev/null >/dev/null 2>&1 & sleep 10",
            directory.path(),
            limits(),
            token.clone(),
        );
        wait_ready(directory.path()).await;
        let waiting = tokio::spawn(process.wait());
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(
            !token.is_cancelled(),
            "transport drop must not cancel its parent"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while escaped_pid_path(directory.path()).exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!directory.path().join("late").exists());
    }

    #[tokio::test]
    async fn supervisor_crash_cannot_mint_a_settlement_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let process = spawn(
            "kill -KILL $PPID; exit 0",
            directory.path(),
            limits(),
            CancellationToken::new(),
        );
        let outcome = process.wait().await.unwrap();
        assert!(outcome.settlement.is_none(), "{outcome:?}");
        assert!(!outcome.status.unwrap().success());
    }
}
