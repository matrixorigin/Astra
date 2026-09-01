#[cfg(target_os = "linux")]
mod linux {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Duration;

    fn spawn_real_astra_supervisor(
        target_program: &str,
        target_args: &[String],
        cwd: &std::path::Path,
    ) -> (std::process::Child, astra_sandbox::InvocationSupervisor) {
        let (mut command, mut supervisor) =
            astra_sandbox::InvocationSupervisor::prepare_with_helper_command(
                std::path::PathBuf::from(env!("CARGO_BIN_EXE_astra")),
                std::iter::empty::<String>(),
                target_program,
                target_args,
            )
            .expect("prepare supervisor protocol");
        command
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        supervisor
            .install(&mut command)
            .expect("install supervisor protocol");
        let mut child = command.spawn().expect("spawn real Astra helper");
        let helper_pid = child.id();
        supervisor.spawned();
        if let Err(error) = supervisor.start(helper_pid) {
            let _ = supervisor.request_termination();
            let _ = child.wait();
            panic!("real Astra helper handshake failed: {error}");
        }
        (child, supervisor)
    }

    #[test]
    fn real_astra_early_entrypoint_contains_daemonized_late_writer() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("mutation.txt");
        let command = format!(
            "printf immediate > {path}; \
             setsid /bin/sh -c '/bin/sh -c \"sleep 0.35; printf late >> {path}\" \
             </dev/null >/dev/null 2>&1 & exit 0' </dev/null >/dev/null 2>&1 & exit 0",
            path = marker.display()
        );
        let args = vec!["-c".to_string(), command];
        let (mut child, mut supervisor) = spawn_real_astra_supervisor("/bin/sh", &args, dir.path());
        let helper_pid = child.id();

        assert!(child.wait().unwrap().success());
        assert!(supervisor.finish(helper_pid));
        std::thread::sleep(Duration::from_millis(450));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "immediate");
    }
}
