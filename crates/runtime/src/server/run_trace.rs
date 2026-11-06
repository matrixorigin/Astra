//! Background `astra.run` and the inbound carrier captured on the request task.
//!
//! `spawn_observed` does not inherit the request span. The HTTP handler reads
//! the inbound carrier before spawn and binds `astra.run` inside that task.
//! A user-input wait ends the span; the continuation starts a new trace.

use std::cell::RefCell;
use std::future::Future;

use astra_logging::trace_context::{Carrier, RunSpan};

tokio::task_local! {
    static INBOUND: Carrier;
    static ACTIVE_RUN: RefCell<RunSpan>;
}

pub(crate) async fn scope_inbound<F>(carrier: Carrier, future: F) -> F::Output
where
    F: Future,
{
    INBOUND.scope(carrier, future).await
}

pub(crate) fn inbound_carrier() -> Carrier {
    INBOUND.try_with(Clone::clone).unwrap_or_default()
}

pub(crate) async fn bind<F>(carrier: Carrier, future: F) -> F::Output
where
    F: Future,
{
    ACTIVE_RUN
        .scope(RefCell::new(RunSpan::start(&carrier)), future)
        .await
}

/// Close `astra.run` before the task parks on user input.
pub(crate) fn suspend_for_user_input() -> UserInputTraceGuard {
    let _ = ACTIVE_RUN.try_with(|slot| slot.borrow_mut().end());
    UserInputTraceGuard
}

pub(crate) struct UserInputTraceGuard;

impl Drop for UserInputTraceGuard {
    fn drop(&mut self) {
        let _ = ACTIVE_RUN.try_with(|slot| {
            let mut slot = slot.borrow_mut();
            if !slot.is_open() {
                *slot = RunSpan::start(&Carrier::default());
            }
        });
    }
}

pub(crate) fn parent_to_active_run(span: &tracing::Span) {
    let link = ACTIVE_RUN
        .try_with(|slot| slot.borrow().link())
        .ok()
        .flatten();
    if let Some(link) = link {
        astra_logging::trace_context::attach_link(span, &link);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inbound_carrier_stays_on_the_request_task() {
        let carrier = Carrier::from_header_values(
            Some("00-0123456789abcdef0123456789abcdef-0123456789abcdef-01"),
            Some("vendor=value"),
        );
        scope_inbound(carrier.clone(), async {
            assert_eq!(inbound_carrier(), carrier);
        })
        .await;
        assert_eq!(inbound_carrier(), Carrier::default());
    }
}
