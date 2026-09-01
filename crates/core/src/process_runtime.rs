//! Process-level Tokio runtime contract shared by the CLI and standalone server.
//!
//! Astra's server-owned agent loop polls deeply composed futures across model,
//! tool, policy, persistence, and SSE boundaries. The platform default worker
//! stack is not a sufficient production contract for those paths: a workload-
//! dependent overflow aborts the whole process rather than returning an error.

/// Explicit worker stack budget for Astra process runtimes.
///
/// Stack pages are committed on demand; this is primarily a virtual-address
/// reservation per worker. Keep the value shared so `astra serve`, the
/// standalone server, and embedded CLI behavior do not diverge.
pub const PROCESS_WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

pub fn build_process_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(PROCESS_WORKER_STACK_BYTES)
        .thread_name("astra-worker")
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_process_runtime_executes_spawned_work() {
        let runtime = build_process_runtime().expect("process runtime");
        let answer = runtime.block_on(async {
            tokio::spawn(async { 42_u8 })
                .await
                .expect("worker task must join")
        });
        assert_eq!(answer, 42);
        assert_eq!(PROCESS_WORKER_STACK_BYTES, 16 * 1024 * 1024);
    }
}
