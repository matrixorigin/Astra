//! Background `astra.run` and the inbound carrier captured on the request task.
//!
//! `spawn_observed` does not inherit the request span. The HTTP handler reads
//! the inbound carrier before spawn and binds `astra.run` inside that task.
//! A user-input wait ends the span; the continuation starts a new trace.
//! A new task does not inherit the run slot, so owned inference tasks use
//! [`in_active_run`] or [`spawn_in_active_run`] and only observe the parent span.

use std::future::Future;

use astra_logging::trace_context::Carrier;

tokio::task_local! {
    static INBOUND: Carrier;
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

pub(crate) fn bind<F>(carrier: Carrier, future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    astra_logging::trace_context::bind_run(&carrier, future)
}

/// Close `astra.run` before the task parks on user input.
pub(crate) fn suspend_for_user_input() -> UserInputTraceGuard {
    astra_logging::trace_context::end_active_run();
    UserInputTraceGuard
}

pub(crate) struct UserInputTraceGuard;

impl Drop for UserInputTraceGuard {
    fn drop(&mut self) {
        astra_logging::trace_context::reopen_active_run();
    }
}

pub(crate) fn parent_to_active_run(span: &tracing::Span) {
    astra_logging::trace_context::parent_to_active_run(span);
}

pub(crate) fn fail_active_run(code: &'static str) {
    astra_logging::trace_context::fail_active_run(code);
}

pub(crate) fn note_attempt_code(code: &'static str) {
    astra_logging::trace_context::note_attempt_code(code);
}

pub(crate) fn note_attempt_settlement_error(code: &'static str) {
    astra_logging::trace_context::note_attempt_settlement_error(code);
}

pub(crate) fn attempt_settlement_error() -> Option<&'static str> {
    astra_logging::trace_context::attempt_settlement_error()
}

pub(crate) fn clear_attempt_code() {
    astra_logging::trace_context::clear_attempt_code();
}

pub(crate) fn attempt_code() -> Option<&'static str> {
    astra_logging::trace_context::attempt_code()
}

pub(crate) fn in_active_run<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    astra_logging::trace_context::in_active_run(future)
}

pub(crate) fn spawn_in_active_run<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    astra_logging::trace_context::spawn_in_active_run(future)
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
