//! Turn execution pipeline: auth retry, stream running, settlement, commit,
//! reporting, and post-commit side effects.

pub mod local_run_control;
pub mod turn_auth_retry;
pub mod turn_cancellation;
pub mod turn_commit;
pub mod turn_entry;
pub mod turn_facade;
pub mod turn_failure_reporting;
pub mod turn_learning;
pub mod turn_post_commit;
pub mod turn_reporting;
pub mod turn_retry;
pub mod turn_session_retry;
pub mod turn_settlement;
pub mod turn_stream_runner;
pub mod turn_success;

pub(crate) use turn_facade::execute_basic_cli_turn;
