//! Reusable local Runner boundaries. Hosting does not confer Server admission
//! authority, and provider credentials never enter Server-facing configuration.

pub mod inference_connection;
pub mod inference_host;
mod inference_journal;
