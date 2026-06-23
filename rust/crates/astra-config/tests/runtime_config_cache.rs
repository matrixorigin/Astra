//! P1 contract: `RuntimeConfig::cached()` returns the same instance across
//! calls — one TOML parse per process, not per turn.

use astra_config::runtime_config::RuntimeConfig;

#[test]
fn cached_returns_same_instance() {
    let a = RuntimeConfig::cached() as *const RuntimeConfig;
    let b = RuntimeConfig::cached() as *const RuntimeConfig;
    assert_eq!(a, b, "cached() must return the same &'static instance");
}

#[test]
fn cached_is_deserializable_as_expected() {
    // Sanity — cached() should match a fresh load() modulo cache (they
    // are logically the same config; this is just confirming the cache
    // isn't returning corrupted/zeroed data).
    let cached = RuntimeConfig::cached();
    let fresh = RuntimeConfig::load();
    // Compare a couple of fields that shouldn't be affected by timing.
    assert_eq!(cached.version, fresh.version);
    assert_eq!(
        cached.tool_surface.always_load_tools,
        fresh.tool_surface.always_load_tools
    );
}
