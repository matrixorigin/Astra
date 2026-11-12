//! Provider wire artifacts and decoding shared by inference executors.
//!
//! This crate has no credential, routing, lifecycle, database, or UI authority.
//! Compilation here serializes the final provider JSON; canonical message and
//! tool projection remain owned by the runtime until their separate extraction.

pub mod openai;
pub mod request;
pub mod sse;
pub mod transport;

pub use request::{ExactProviderRequest, ProviderProtocol, RequestCompileError, RequestIdentity};

/// Default admitted request artifact size. Larger limits require an explicitly
/// admitted capacity profile; transport cannot silently increase this value.
pub const DEFAULT_REQUEST_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum retained incomplete provider SSE event by default. Completed events
/// are delivered incrementally and do not count against later events.
pub const DEFAULT_SSE_EVENT_LIMIT_BYTES: usize = 16 * 1024 * 1024;
