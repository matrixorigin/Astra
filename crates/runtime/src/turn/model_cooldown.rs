//! Process-wide model cooldown registry.
//!
//! Provider transport and retries live in `turn::llm::client`.

use astra_turn_core::rate_limit_cooldown::PerModelCooldown;
use std::sync::OnceLock;

pub(crate) fn rate_limit_cooldown() -> &'static PerModelCooldown {
    static COOLDOWN: OnceLock<PerModelCooldown> = OnceLock::new();
    COOLDOWN.get_or_init(PerModelCooldown::new)
}
