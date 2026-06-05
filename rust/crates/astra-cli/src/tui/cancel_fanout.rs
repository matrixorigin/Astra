//! Cancel fan-out helper: given a list of in-flight task tool_use_ids,
//! invoke a cancel callback for each one.
//!
//! The real wire target is
//! [`astra_services::TaskService::update_status`] with
//! `TaskStatus::Cancelled`, but callers pass a closure so unit tests
//! can record calls without touching the task service.
//!
//! Two invariants worth protecting:
//!
//! 1. **Every id is attempted.** A single failing cancel must not
//!    short-circuit the rest; Ctrl+C's whole point is to clean up
//!    the mess, not half of it. We collect errors and return them.
//! 2. **Idempotence across retries.** Ctrl+C is often pressed twice
//!    rapidly; the caller is responsible for deduping or letting
//!    the service's own idempotence handle it. We don't memoise
//!    here because the id list is the source of truth — the TUI
//!    already prunes it on `ToolCompleted`.

#[cfg(not(test))]
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const CANCEL_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

/// Run `cancel` once per id, collect any errors, return them. All cancels are
/// attempted concurrently so one hung backend cannot freeze Ctrl+C handling.
pub(crate) async fn fanout<F, Fut>(ids: &[String], cancel: F) -> Vec<(String, String)>
where
    F: Fn(String) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let mut joins = tokio::task::JoinSet::new();
    for id in ids {
        let cancel = cancel.clone();
        let id = id.clone();
        joins.spawn(async move {
            match tokio::time::timeout(CANCEL_TIMEOUT, cancel(id.clone())).await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some((id, e)),
                Err(_) => Some((id, format!("cancel timed out after {:?}", CANCEL_TIMEOUT))),
            }
        });
    }

    let mut errs = Vec::new();
    while let Some(joined) = joins.join_next().await {
        match joined {
            Ok(Some(err)) => errs.push(err),
            Ok(None) => {}
            Err(e) => errs.push(("<join>".to_string(), format!("cancel task join error: {e}"))),
        }
    }
    errs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock_recovery::LockRecovery;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[tokio::test]
    async fn empty_list_is_a_noop() {
        let called = Arc::new(Mutex::new(Vec::<String>::new()));
        let c = called.clone();
        let errs = fanout(&[], move |id| {
            let c = c.clone();
            async move {
                c.lock_recover().push(id);
                Ok(())
            }
        })
        .await;
        assert!(errs.is_empty());
        assert!(called.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn every_id_gets_invoked_exactly_once() {
        // Note: `JoinSet` schedules tasks concurrently, so observed
        // invocation order is non-deterministic. We assert the *set*
        // of invoked ids, not their order. (Earlier this test was
        // named `…_in_order` but sorted before comparing — the name
        // overstated what the assertion actually verified.)
        let ids: Vec<String> = vec!["tu_1".into(), "tu_2".into(), "tu_3".into()];
        let called = Arc::new(Mutex::new(Vec::<String>::new()));
        let c = called.clone();
        let errs = fanout(&ids, move |id| {
            let c = c.clone();
            async move {
                c.lock_recover().push(id);
                Ok(())
            }
        })
        .await;
        assert!(errs.is_empty());
        let mut actual = called.lock_recover().clone();
        actual.sort();
        assert_eq!(
            actual, ids,
            "every id must be invoked exactly once (order not asserted)"
        );
    }

    #[tokio::test]
    async fn failing_cancel_does_not_abort_the_rest() {
        // Middle cancel errors — first and third must still run.
        // The whole point of Ctrl+C fan-out is to cancel every
        // in-flight task, not bail on the first service hiccup.
        let ids: Vec<String> = vec!["tu_1".into(), "tu_2".into(), "tu_3".into()];
        let called = Arc::new(Mutex::new(Vec::<String>::new()));
        let c = called.clone();
        let errs = fanout(&ids, move |id| {
            let c = c.clone();
            async move {
                c.lock_recover().push(id.clone());
                if id == "tu_2" {
                    Err("service unavailable".into())
                } else {
                    Ok(())
                }
            }
        })
        .await;
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, "tu_2");
        let mut actual = called.lock_recover().clone();
        actual.sort();
        assert_eq!(actual, vec!["tu_1", "tu_2", "tu_3"]);
    }

    #[tokio::test]
    async fn errors_accumulate_with_ids_for_logging() {
        // Multiple failures — caller surfaces each for logging.
        // Test pins the shape (id, message) because the outer
        // event loop formats these into a single banner line.
        let ids: Vec<String> = vec!["tu_1".into(), "tu_2".into()];
        let errs = fanout(&ids, |id| async move {
            Err::<(), String>(format!("cancel failed for {id}"))
        })
        .await;
        assert_eq!(errs.len(), 2);
        let mut errs = errs;
        errs.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(errs[0].0, "tu_1");
        assert!(errs[0].1.contains("tu_1"));
        assert_eq!(errs[1].0, "tu_2");
    }

    #[tokio::test]
    async fn hung_cancel_times_out_without_blocking_other_ids() {
        let ids: Vec<String> = vec!["slow".into(), "fast".into()];
        let called = Arc::new(Mutex::new(Vec::<String>::new()));
        let c = called.clone();
        let start = std::time::Instant::now();
        let errs = fanout(&ids, move |id| {
            let c = c.clone();
            async move {
                c.lock_recover().push(id.clone());
                if id == "slow" {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
                Ok(())
            }
        })
        .await;
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].0, "slow");
        assert!(errs[0].1.contains("timed out"));
    }
}
