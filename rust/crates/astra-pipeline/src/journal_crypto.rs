//! Re-export of the shared local checkpoint/session artifact crypto.
//!
//! Kept under `astra_pipeline::journal_crypto` for existing callers; the
//! implementation lives in `astra_services` so restore/versioning services and
//! pipeline checkpoint writers cannot drift.

pub use astra_services::checkpoint_crypto::{JournalCrypto, hex_decode, hex_encode};
