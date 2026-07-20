//! Conflict-resolution contracts used by team orchestration.
//!
//! Concrete resolvers are injected by the hosting surface. This module does
//! not own model routing or credentials and therefore must not call a provider
//! directly.

pub use astra_server_types::conflict_resolver::*;
