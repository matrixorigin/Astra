//! Serialize cancellation with the start of credential exchange. Once an
//! exchange starts, wait for its bounded result instead of reporting a false
//! cancellation after credentials may already have been committed.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Default)]
pub(super) struct LoginControl(AtomicU8);

impl LoginControl {
    pub(super) fn reset(&self) {
        self.0.store(0, Ordering::SeqCst);
    }

    pub(super) fn begin_exchange(&self) -> Result<(), String> {
        self.0
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| ())
            .map_err(|_| "Login cancelled".to_string())
    }

    pub(super) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst) == 2
    }

    /// Cancel only pre-exchange work. The owner must drain and settle any
    /// committed exchange results instead of dropping successful credentials.
    pub(super) fn prepare_shutdown<T: Send + 'static>(&self, tasks: &mut tokio::task::JoinSet<T>) {
        if self.cancel() {
            tasks.abort_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_browser_cannot_start_credential_exchange() {
        let control = LoginControl::default();
        assert!(control.cancel());
        assert!(control.is_cancelled());
        assert!(control.begin_exchange().is_err());
        control.reset();
        assert!(!control.is_cancelled());
        assert!(control.begin_exchange().is_ok());
    }

    #[test]
    fn committing_login_cannot_be_reported_as_cancelled() {
        let control = LoginControl::default();
        assert!(control.begin_exchange().is_ok());
        assert!(!control.cancel());
        assert!(!control.is_cancelled());
    }

    #[tokio::test]
    async fn shutdown_joins_an_exchange_instead_of_aborting_it() {
        let control = std::sync::Arc::new(LoginControl::default());
        control.begin_exchange().unwrap();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = tokio::task::JoinSet::new();
        let task_release = release.clone();
        let task_completed = completed.clone();
        tasks.spawn(async move {
            task_release.notified().await;
            task_completed.store(true, Ordering::SeqCst);
        });
        let shutdown = tokio::spawn(async move {
            control.prepare_shutdown(&mut tasks);
            while tasks.join_next().await.is_some() {}
        });
        tokio::task::yield_now().await;
        assert!(!shutdown.is_finished());
        release.notify_one();
        shutdown.await.unwrap();
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn exchange_and_cancellation_have_exactly_one_winner() {
        for _ in 0..100 {
            let control = std::sync::Arc::new(LoginControl::default());
            let other = control.clone();
            let exchange = std::thread::spawn(move || other.begin_exchange().is_ok());
            let cancelled = control.cancel();
            assert_ne!(cancelled, exchange.join().unwrap());
        }
    }
}
