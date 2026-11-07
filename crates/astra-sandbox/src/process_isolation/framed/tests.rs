use super::*;
use crate::CgroupGuard;

fn limits() -> ProcessIoLimits {
    ProcessIoLimits {
        max_frame_bytes: 64,
        max_queued_frames: 1,
        max_stderr_bytes: 16,
        timeout: Some(Duration::from_secs(5)),
    }
}

#[tokio::test]
async fn input_cancellation_distinguishes_before_and_after_submission() {
    let (sender, mut receiver) = mpsc::channel(1);
    let input = ProcessInput {
        sender: sender.clone(),
        cancel: CancellationToken::new(),
        max_frame_bytes: 64,
        framed: false,
    };
    sender
        .send(InputWrite {
            bytes: None,
            written: None,
        })
        .await
        .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(matches!(
        input
            .write_bytes(b"before", deadline, Some(&cancelled))
            .await,
        Err(ProcessInputError::NotSent(_))
    ));
    receiver.recv().await.unwrap();

    let cancelled = CancellationToken::new();
    let writer = {
        let (input, cancelled) = (input.clone(), cancelled.clone());
        tokio::spawn(async move {
            input
                .write_bytes(b"after", deadline, Some(&cancelled))
                .await
        })
    };
    // Receiving the queued frame proves submission happened; retain its ack
    // sender so cancellation happens before an acknowledgement is possible.
    let submitted = receiver.recv().await.unwrap();
    cancelled.cancel();
    assert!(matches!(
        writer.await.unwrap(),
        Err(ProcessInputError::Unconfirmed(_))
    ));
    drop(submitted);
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
        ProcessIoLimits {
            max_frame_bytes: 0,
            ..limits()
        },
        ProcessIoLimits {
            max_queued_frames: 0,
            ..limits()
        },
        ProcessIoLimits {
            max_queued_frames: usize::MAX,
            ..limits()
        },
        ProcessIoLimits {
            max_stderr_bytes: usize::MAX,
            ..limits()
        },
        ProcessIoLimits {
            timeout: Some(Duration::ZERO),
            ..limits()
        },
    ] {
        assert_eq!(
            budget.deadline().unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[cfg(unix)]
fn spawn_bytes(script: &str, stdin: bool, budget: ProcessIoLimits) -> OwnedProcess {
    let owner = BashInvocationOwner {
        process_scope: CgroupGuard {
            cg_path: None,
            procs_path: None,
        },
        supervisor: None,
    };
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    owner
        .spawn_streamed(command, budget, CancellationToken::new(), stdin, ())
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn byte_stream_keeps_process_identity_and_acknowledges_targeted_input() {
    let mut process = spawn_bytes(
        "printf '%s\\n' $$; read -r line; printf 'pid:%s out:%s' $$ \"$line\"; printf 'err' >&2",
        true,
        limits(),
    );
    let mut first = Vec::new();
    while !first.contains(&b'\n') {
        match process.recv_output().await.unwrap() {
            ProcessOutput::Stdout(bytes) => first.extend(bytes),
            other => panic!("unexpected initial output: {other:?}"),
        }
    }
    let pid = std::str::from_utf8(&first)
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    process
        .input()
        .unwrap()
        .write_bytes(
            b"answer\n",
            tokio::time::Instant::now() + Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(output) = process.recv_output().await {
        match output {
            ProcessOutput::Stdout(bytes) => stdout.extend(bytes),
            ProcessOutput::Stderr(bytes) => stderr.extend(bytes),
        }
    }
    let outcome = process.wait().await.unwrap();
    assert!(matches!(outcome.end, ProcessEnd::Exited));
    assert!(outcome.status.unwrap().success());
    assert_eq!(stdout, format!("pid:{pid} out:answer").as_bytes());
    assert_eq!(stderr, b"err");
    assert!(
        outcome
            .settlement
            .is_none_or(|settlement| !settlement.ownership.is_authoritative()),
        "a process group is not descendant proof"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn byte_stream_stdin_is_closed_unless_explicitly_requested() {
    let mut process = spawn_bytes(
        "if read line; then printf stolen; else printf closed; fi",
        false,
        limits(),
    );
    assert!(process.input().is_none());
    let mut output = Vec::new();
    while let Some(ProcessOutput::Stdout(bytes)) = process.recv_output().await {
        output.extend(bytes);
    }
    assert_eq!(output, b"closed");
    assert!(process.wait().await.unwrap().status.unwrap().success());
}

#[cfg(unix)]
#[tokio::test]
async fn byte_stream_eof_and_deadline_are_physical_process_facts() {
    let mut process = spawn_bytes("cat; printf eof", true, limits());
    process
        .input()
        .unwrap()
        .write_bytes(
            b"raw\0bytes\n",
            tokio::time::Instant::now() + Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    process
        .input()
        .unwrap()
        .close(tokio::time::Instant::now() + Duration::from_secs(5), None)
        .await
        .unwrap();
    let mut output = Vec::new();
    while let Some(ProcessOutput::Stdout(bytes)) = process.recv_output().await {
        output.extend(bytes);
    }
    assert_eq!(output, b"raw\0bytes\neof");
    assert!(process.wait().await.unwrap().status.unwrap().success());

    let process = spawn_bytes(
        "read line",
        true,
        ProcessIoLimits {
            timeout: Some(Duration::from_millis(100)),
            ..limits()
        },
    );
    let _input = process.input().unwrap();
    let outcome = process.wait().await.unwrap();
    assert!(matches!(outcome.end, ProcessEnd::TimedOut), "{outcome:?}");
    assert!(
        outcome.status.is_some(),
        "timeout waits for the observed process exit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_unblocks_full_byte_stream_queues() {
    let process = spawn_bytes("while :; do printf flood; done", false, limits());
    let outcome = tokio::time::timeout(Duration::from_secs(5), process.cancel_and_wait())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(outcome.end, ProcessEnd::Cancelled));
    assert!(outcome.status.is_some());
}

#[tokio::test]
async fn cancelled_queued_input_is_not_delivered() {
    let (tx, rx) = mpsc::channel(1);
    let (ack, acknowledgement) = oneshot::channel();
    tx.send(InputWrite {
        bytes: Some(b"must not run".to_vec()),
        written: Some(ack),
    })
    .await
    .unwrap();
    drop(acknowledgement);
    drop(tx);
    let (writer, mut reader) = tokio::io::duplex(64);
    write_frames(writer, rx, false).await.unwrap();
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await.unwrap();
    assert!(bytes.is_empty());
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    fn spawn(
        script: &str,
        directory: &std::path::Path,
        budget: ProcessIoLimits,
        cancel: CancellationToken,
    ) -> OwnedProcess {
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
        budget: ProcessIoLimits,
        cancel: CancellationToken,
        helper: &str,
    ) -> OwnedProcess {
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
    async fn next_payload(process: &mut OwnedProcess) -> Vec<u8> {
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

    fn assert_authority(outcome: &ProcessOutcome) {
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

    #[tokio::test]
    async fn unbudgeted_process_survives_day_wait_and_settles_after_input() {
        let directory = tempfile::tempdir().unwrap();
        let mut process = spawn(
            "touch ready; read line; printf 'finished\\n'",
            directory.path(),
            ProcessIoLimits {
                timeout: None,
                ..limits()
            },
            CancellationToken::new(),
        );
        // Finish the real ownership handshake before advancing Tokio time.
        wait_ready(directory.path()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(86_400)).await;
        tokio::time::resume();
        process
            .input()
            .unwrap()
            .send_frame(b"continue")
            .await
            .unwrap();
        assert_eq!(next_payload(&mut process).await, b"finished");
        let outcome = tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(outcome.end, ProcessEnd::Exited), "{outcome:?}");
        assert_authority(&outcome);
    }

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
        assert!(matches!(outcome.end, ProcessEnd::Cancelled), "{outcome:?}");
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
        assert!(matches!(outcome.end, ProcessEnd::Cancelled), "{outcome:?}");
        assert_eq!(outcome.target_released, Some(false));
        assert!(outcome.settlement.is_none());
        assert!(!directory.path().join("started").exists());
    }

    #[tokio::test]
    async fn startup_deadline_waits_for_owned_cleanup_without_releasing_target() {
        let directory = tempfile::tempdir().unwrap();
        let budget = ProcessIoLimits {
            timeout: Some(Duration::from_millis(50)),
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
        tokio::time::sleep(budget.timeout.unwrap() + Duration::from_millis(10)).await;
        std::fs::write(
            directory.path().join("release-helper"),
            b"release after deadline",
        )
        .unwrap();
        let outcome = process.wait().await.unwrap();
        assert!(matches!(outcome.end, ProcessEnd::TimedOut), "{outcome:?}");
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
        let input = process.input().unwrap();
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
        let input = process.input().unwrap();
        let first = vec![b'x'; 64];
        input.send_frame(&first).await.unwrap();
        assert_eq!(next_payload(&mut process).await, first);
        input.send_frame(br#"{"second":true}"#).await.unwrap();
        assert_eq!(next_payload(&mut process).await, br#"{"second":true}"#);
        drop(input);
        let outcome = process.wait().await.unwrap();
        assert!(matches!(outcome.end, ProcessEnd::Exited), "{outcome:?}");
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
        let input = process.input().unwrap();
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
        assert!(matches!(outcome.end, ProcessEnd::Exited), "{outcome:?}");
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
            matches!(&outcome.end, ProcessEnd::OutputFailed(error) if error.kind() == io::ErrorKind::InvalidData),
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
            matches!(&outcome.end, ProcessEnd::OutputFailed(error) if error.kind() == io::ErrorKind::UnexpectedEof),
            "{outcome:?}"
        );
        assert_authority(&outcome);
    }

    #[tokio::test]
    async fn blocked_stdin_and_full_input_queue_are_cancelled_and_settled() {
        let directory = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let budget = ProcessIoLimits {
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
        let input = process.input().unwrap();
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
        assert!(matches!(outcome.end, ProcessEnd::Cancelled), "{outcome:?}");
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
        assert!(matches!(outcome.end, ProcessEnd::Cancelled), "{outcome:?}");
        assert_authority(&outcome);
        assert!(!escaped_pid_path(directory.path()).exists());
        assert!(!directory.path().join("late").exists());
    }

    #[tokio::test]
    async fn deadline_terminates_a_process_with_blocked_output() {
        let directory = tempfile::tempdir().unwrap();
        let budget = ProcessIoLimits {
            timeout: Some(Duration::from_secs(3)),
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
        assert!(matches!(outcome.end, ProcessEnd::TimedOut), "{outcome:?}");
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

#[cfg(unix)]
#[tokio::test]
async fn raw_closed_stdin_does_not_cancel_still_useful_work() {
    let mut process = spawn_bytes(
        "exec 0<&-; printf 'stdin-closed\\n'; sleep 0.05; printf 'finished\\n'",
        true,
        limits(),
    );
    let input = process.input().unwrap();
    let first = process.recv_output().await.unwrap();
    assert!(
        matches!(first, ProcessOutput::Stdout(ref bytes) if bytes.starts_with(b"stdin-closed"))
    );
    assert!(
        input
            .write_bytes(
                b"not delivered",
                tokio::time::Instant::now() + Duration::from_secs(5),
                None
            )
            .await
            .is_err()
    );
    let mut output = Vec::new();
    while let Some(part) = process.recv_output().await {
        if let ProcessOutput::Stdout(bytes) = part {
            output.extend(bytes);
        }
    }
    let terminal = process.wait().await.unwrap();
    assert!(matches!(terminal.end, ProcessEnd::Exited));
    assert!(terminal.status.unwrap().success());
    assert!(String::from_utf8(output).unwrap().contains("finished"));
    assert!(!input.is_open());
}
