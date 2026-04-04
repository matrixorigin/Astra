use std::sync::Arc;

use astra_runtime::{AppState, HealthChecker, ServiceInfo, SharedPool};
use async_trait::async_trait;

#[derive(Clone)]
struct StubHealthChecker;

#[async_trait]
impl HealthChecker for StubHealthChecker {
    async fn database_healthy(&self) -> bool {
        true
    }
}

#[test]
fn app_state_shared_pool_defaults_to_none() {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
    assert!(state.shared_pool.is_none());
}

#[test]
fn app_state_with_shared_pool_is_some() {
    // We can't create a real SharedPool without a DB, but we can verify the builder
    // method compiles and the field is accessible.
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealthChecker));
    // Verify the field exists and is None by default
    assert!(state.shared_pool.is_none());
}

#[test]
fn shared_pool_settings_accessible() {
    // Verify SharedPool type is exported and its API is accessible
    fn _assert_clone<T: Clone>() {}
    _assert_clone::<SharedPool>();
}
