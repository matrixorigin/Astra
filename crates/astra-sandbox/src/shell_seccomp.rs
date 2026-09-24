//! Native x86-64 only: reject compat and x32 ABIs before interpreting syscall
//! numbers. Default ENOSYS also closes future syscall and io_uring bypasses.
use std::io;

pub(crate) fn policy() -> io::Result<Vec<u8>> {
    #[cfg(not(target_arch = "x86_64"))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "restricted shell seccomp currently supports Linux x86-64 only",
    ));
    #[cfg(target_arch = "x86_64")]
    {
        // seccomp_data: nr at 0, audit architecture at 4. Instruction encoding
        // is the kernel's native struct sock_filter layout (u16,u8,u8,u32).
        let mut result = Vec::new();
        let mut instruction = |code: u16, jt: u8, jf: u8, k: u32| {
            result.extend_from_slice(&code.to_ne_bytes());
            result.extend_from_slice(&[jt, jf]);
            result.extend_from_slice(&k.to_ne_bytes());
        };
        instruction(0x20, 0, 0, 4); // LD W ABS arch
        instruction(0x15, 1, 0, 0xc000003e); // AUDIT_ARCH_X86_64
        instruction(0x06, 0, 0, 0x80000000); // KILL_PROCESS
        instruction(0x20, 0, 0, 0); // LD W ABS nr
        instruction(0x35, 0, 1, 0x40000000); // reject x32 (and negative nr)
        instruction(0x06, 0, 0, 0x80000000);
        // No socket/socketpair/connect/send*/recv*, socketcall, IPC, keyring,
        // io_uring, ptrace, process_vm_*, pidfd_getfd, bpf, perf, mount, mknod,
        // namespace creation via unshare/setns, or device ioctl operations.
        // clone is handled below; clone3 is ENOSYS so libc can use clone.
        let allowed = [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_readv,
            libc::SYS_writev,
            libc::SYS_pread64,
            libc::SYS_pwrite64,
            libc::SYS_close,
            libc::SYS_close_range,
            libc::SYS_lseek,
            libc::SYS_fstat,
            libc::SYS_stat,
            libc::SYS_lstat,
            libc::SYS_newfstatat,
            libc::SYS_statx,
            libc::SYS_open,
            libc::SYS_openat,
            libc::SYS_access,
            libc::SYS_faccessat,
            libc::SYS_faccessat2,
            libc::SYS_readlink,
            libc::SYS_readlinkat,
            libc::SYS_getdents,
            libc::SYS_getdents64,
            libc::SYS_getcwd,
            libc::SYS_chdir,
            libc::SYS_fchdir,
            libc::SYS_dup,
            libc::SYS_dup2,
            libc::SYS_dup3,
            libc::SYS_fcntl,
            libc::SYS_pipe,
            libc::SYS_pipe2,
            libc::SYS_poll,
            libc::SYS_ppoll,
            libc::SYS_select,
            libc::SYS_pselect6,
            libc::SYS_epoll_create,
            libc::SYS_epoll_create1,
            libc::SYS_epoll_ctl,
            libc::SYS_epoll_wait,
            libc::SYS_epoll_pwait,
            libc::SYS_eventfd2,
            libc::SYS_signalfd4,
            libc::SYS_mmap,
            libc::SYS_mprotect,
            libc::SYS_munmap,
            libc::SYS_mremap,
            libc::SYS_madvise,
            libc::SYS_brk,
            libc::SYS_msync,
            libc::SYS_mincore,
            libc::SYS_futex,
            libc::SYS_set_robust_list,
            libc::SYS_get_robust_list,
            libc::SYS_rseq,
            libc::SYS_set_tid_address,
            libc::SYS_arch_prctl,
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_rt_sigreturn,
            libc::SYS_sigaltstack,
            libc::SYS_rt_sigpending,
            libc::SYS_rt_sigsuspend,
            libc::SYS_rt_sigtimedwait,
            libc::SYS_kill,
            libc::SYS_tkill,
            libc::SYS_tgkill,
            libc::SYS_fork,
            libc::SYS_vfork,
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_wait4,
            libc::SYS_waitid,
            libc::SYS_exit,
            libc::SYS_exit_group,
            libc::SYS_getpid,
            libc::SYS_getppid,
            libc::SYS_gettid,
            libc::SYS_getuid,
            libc::SYS_geteuid,
            libc::SYS_getgid,
            libc::SYS_getegid,
            libc::SYS_getresuid,
            libc::SYS_getresgid,
            libc::SYS_getgroups,
            libc::SYS_getpgrp,
            libc::SYS_getpgid,
            libc::SYS_setpgid,
            libc::SYS_getsid,
            libc::SYS_setsid,
            libc::SYS_uname,
            libc::SYS_getrandom,
            libc::SYS_clock_gettime,
            libc::SYS_clock_getres,
            libc::SYS_clock_nanosleep,
            libc::SYS_nanosleep,
            libc::SYS_gettimeofday,
            libc::SYS_time,
            libc::SYS_times,
            libc::SYS_alarm,
            libc::SYS_setitimer,
            libc::SYS_getitimer,
            libc::SYS_getrusage,
            libc::SYS_getrlimit,
            libc::SYS_prlimit64,
            libc::SYS_sched_yield,
            libc::SYS_sched_getaffinity,
            libc::SYS_sched_getparam,
            libc::SYS_sched_getscheduler,
            libc::SYS_getpriority,
            libc::SYS_setpriority,
            libc::SYS_umask,
            libc::SYS_mkdir,
            libc::SYS_mkdirat,
            libc::SYS_rmdir,
            libc::SYS_unlink,
            libc::SYS_unlinkat,
            libc::SYS_rename,
            libc::SYS_renameat,
            libc::SYS_renameat2,
            libc::SYS_link,
            libc::SYS_linkat,
            libc::SYS_symlink,
            libc::SYS_symlinkat,
            libc::SYS_chmod,
            libc::SYS_fchmod,
            libc::SYS_fchmodat,
            libc::SYS_truncate,
            libc::SYS_ftruncate,
            libc::SYS_fallocate,
            libc::SYS_fsync,
            libc::SYS_fdatasync,
            libc::SYS_flock,
            libc::SYS_utime,
            libc::SYS_utimes,
            libc::SYS_utimensat,
            libc::SYS_futimesat,
            libc::SYS_statfs,
            libc::SYS_fstatfs,
            libc::SYS_sendfile,
            libc::SYS_copy_file_range,
            libc::SYS_fadvise64,
            libc::SYS_readahead,
        ];
        for number in allowed {
            instruction(0x15, 0, 1, number as u32);
            instruction(0x06, 0, 0, 0x7fff0000); // ALLOW
        }
        instruction(0x15, 1, 0, libc::SYS_clone as u32);
        instruction(0x06, 0, 0, 0x00050000 | libc::ENOSYS as u32);
        instruction(0x20, 0, 0, 16); // clone flags, args[0] low word
        let forbidden = libc::CLONE_NEWUSER
            | libc::CLONE_NEWNS
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWCGROUP
            | libc::CLONE_UNTRACED;
        instruction(0x45, 0, 1, forbidden as u32); // JSET
        instruction(0x06, 0, 0, 0x00050000 | libc::EPERM as u32);
        instruction(0x06, 0, 0, 0x7fff0000);
        Ok(result)
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::process::Command;

    fn filtered_python(script: &str) -> std::process::Output {
        let filters: Vec<libc::sock_filter> = policy()
            .unwrap()
            .chunks_exact(8)
            .map(|b| libc::sock_filter {
                code: u16::from_ne_bytes(b[0..2].try_into().unwrap()),
                jt: b[2],
                jf: b[3],
                k: u32::from_ne_bytes(b[4..8].try_into().unwrap()),
            })
            .collect();
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c", script]).env_clear();
        // Test the exact exported filter in a real child without requiring
        // mount namespaces. No filtering or unsafe post-fork work in parent.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let program = libc::sock_fprog {
                    len: filters.len() as u16,
                    filter: filters.as_ptr().cast_mut(),
                };
                if libc::prctl(libc::PR_SET_SECCOMP, 2, &program) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command.output().unwrap()
    }

    #[test]
    fn real_filter_denies_native_ipc_and_namespace_entrypoints() {
        let output = filtered_python(
            r#"
import ctypes, errno, os
c = ctypes.CDLL(None, use_errno=True)
# socket, connect, socketpair, keyctl, io_uring, pidfd_getfd, ptrace,
# process_vm_writev, unshare, mount, bpf, clone3 and unknown syscalls.
for nr in [41, 42, 53, 250, 425, 426, 427, 438, 101, 311, 272, 165, 321, 435, 9999]:
    ctypes.set_errno(0)
    assert c.syscall(nr, -1, 0, 0, 0, 0, 0) == -1, nr
    assert ctypes.get_errno() == errno.ENOSYS, (nr, ctypes.get_errno())
# Native clone with CLONE_NEWUSER must fail without creating a child.
assert c.syscall(56, 0x10000000, 0, 0, 0, 0, 0) == -1
assert ctypes.get_errno() == errno.EPERM
pid = os.fork()
if pid == 0: os._exit(7)
assert os.waitpid(pid, 0)[1] == 7 << 8
"#,
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_filter_blocks_pathname_sockets_keyrings_and_io_uring() {
        let root = tempfile::tempdir().unwrap();
        let socket_path = root.path().join("host.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        std::os::unix::net::UnixStream::connect(&socket_path).unwrap();
        let output = filtered_python(&format!(
            r#"
import socket, ctypes, errno, os
assert os.path.exists({path:?})
try:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect({path:?})
except OSError as e: assert e.errno == errno.ENOSYS, e
else: raise AssertionError('host socket reachable')
c = ctypes.CDLL(None, use_errno=True)
# Native connect, socketpair, keyctl, io_uring_setup/enter/register,
# pidfd_getfd, ptrace, process_vm_writev, unshare, mount, bpf and future nr.
for nr in [42, 53, 250, 425, 426, 427, 438, 101, 311, 272, 165, 321, 9999]:
    ctypes.set_errno(0)
    assert c.syscall(nr, -1, 0, 0, 0, 0, 0) == -1, nr
    assert ctypes.get_errno() == errno.ENOSYS, (nr, ctypes.get_errno())
# Normal shell/compiler process primitives remain usable.
pid = os.fork()
if pid == 0: os._exit(7)
assert os.waitpid(pid, 0)[1] == 7 << 8
"#,
            path = socket_path.to_str().unwrap()
        ));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn real_filter_kills_x32_and_i386_syscall_abis() {
        let output = filtered_python("import ctypes; ctypes.CDLL(None).syscall(0x40000027)");
        assert_eq!(output.status.signal(), Some(libc::SIGSYS), "{output:?}");
        // int 0x80 selects AUDIT_ARCH_I386 even from a 64-bit executable.
        let output = filtered_python(
            r#"
import ctypes, mmap
code = mmap.mmap(-1, 4096, prot=mmap.PROT_READ|mmap.PROT_WRITE|mmap.PROT_EXEC)
code.write(bytes.fromhex('b814000000cd80c3'))
ctypes.CFUNCTYPE(ctypes.c_int)(ctypes.addressof(ctypes.c_char.from_buffer(code)))()
"#,
        );
        assert_eq!(output.status.signal(), Some(libc::SIGSYS), "{output:?}");
    }
}
