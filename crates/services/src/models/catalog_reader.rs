//! One authenticated observation boundary. Cold requests load the catalog;
//! subsequent requests reuse a bounded, short-lived principal-isolated cache.
//! Explicit discovery refreshes that same owner. Execution authorization is
//! never cached here.

use std::{sync::Arc, time::Duration};

use astra_core::ErrorResponse;
use axum::{Json, http::StatusCode};

use super::{ModelListItem, ModelService, UserModelCatalog};
use crate::auth::{AuthPrincipal, AuthService};

type CatalogResult = Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)>;

/// Observation cache, not an execution grant. Entries are isolated by the
/// complete authenticated principal and service identity. Per-entry locks
/// coalesce cold reads without serializing unrelated principals' I/O.
#[derive(Clone, Default)]
pub struct AuthorizedModelCatalogCache {
    entries: Arc<tokio::sync::Mutex<Vec<CatalogCacheEntry>>>,
}

struct CatalogCacheEntry {
    principal: AuthPrincipal,
    models: Arc<dyn ModelService>,
    auth: Arc<dyn AuthService>,
    snapshot: Arc<tokio::sync::Mutex<Option<CatalogSnapshot>>>,
}

struct CatalogSnapshot {
    items: Vec<ModelListItem>,
    loaded_at: tokio::time::Instant,
}

impl AuthorizedModelCatalogCache {
    // At most 64 MiB of serialized catalog content across 1,024 principals,
    // plus bounded item/entry overhead. Oversized catalogs are request-local.
    const MAX_ENTRIES: usize = 1024;
    const MAX_ENTRY_BYTES: usize = 64 * 1024;
    const MAX_ENTRY_ITEMS: usize = 1024;
    const TTL: Duration = Duration::from_secs(60);

    async fn snapshot_for(
        &self,
        reader: &AuthorizedModelCatalogReader,
    ) -> Arc<tokio::sync::Mutex<Option<CatalogSnapshot>>> {
        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.iter().find(|entry| {
            entry.principal == reader.principal
                && Arc::ptr_eq(&entry.models, &reader.models)
                && Arc::ptr_eq(&entry.auth, &reader.auth)
        }) {
            return entry.snapshot.clone();
        }
        if entries.len() == Self::MAX_ENTRIES {
            entries.remove(0);
        }
        let snapshot = Arc::new(tokio::sync::Mutex::new(None));
        entries.push(CatalogCacheEntry {
            principal: reader.principal.clone(),
            models: reader.models.clone(),
            auth: reader.auth.clone(),
            snapshot: snapshot.clone(),
        });
        snapshot
    }
}

pub async fn read_authorized_model_catalog(
    models: &dyn ModelService,
    auth: &dyn AuthService,
    principal: &AuthPrincipal,
) -> Result<UserModelCatalog, (StatusCode, Json<ErrorResponse>)> {
    if principal.is_provider_authorized_request() {
        let catalog = auth.external_catalog_by_scope(principal).await?;
        Ok(UserModelCatalog {
            items: catalog
                .models
                .into_iter()
                .map(ModelListItem::from)
                .collect(),
            default_offering_id: catalog.default_model_id,
            allows_deployment: false,
            device_inventory_unavailable: false,
        })
    } else {
        models
            .user_model_catalog(principal.user.user_id.clone())
            .await
    }
}

/// Installed by authentication, not constructed from tool arguments or a
/// workspace identity. Descendants clone the binding, including Edge scope.
#[derive(Clone)]
pub struct AuthorizedModelCatalogReader {
    models: Arc<dyn ModelService>,
    auth: Arc<dyn AuthService>,
    principal: AuthPrincipal,
    cache: AuthorizedModelCatalogCache,
    // One immutable observation (including failure) per request and descendants.
    catalog: Arc<tokio::sync::Mutex<Option<CatalogResult>>>,
}

impl AuthorizedModelCatalogReader {
    pub fn new(
        models: Arc<dyn ModelService>,
        auth: Arc<dyn AuthService>,
        principal: AuthPrincipal,
    ) -> Self {
        Self::with_cache(
            models,
            auth,
            principal,
            AuthorizedModelCatalogCache::default(),
        )
    }

    pub fn with_cache(
        models: Arc<dyn ModelService>,
        auth: Arc<dyn AuthService>,
        principal: AuthPrincipal,
        cache: AuthorizedModelCatalogCache,
    ) -> Self {
        Self {
            models,
            auth,
            principal,
            cache,
            catalog: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.principal.user.user_id
    }

    pub fn scope(&self) -> &'static str {
        if self.principal.is_edge_registration() {
            "edge_registration"
        } else if self.principal.is_provider_authorized_request() {
            "provider_scope"
        } else {
            "user"
        }
    }

    pub fn is_provider_scoped(&self) -> bool {
        self.principal.is_provider_authorized_request()
    }

    async fn read_items(&self) -> CatalogResult {
        if self.principal.is_provider_authorized_request() {
            Ok(self
                .auth
                .external_catalog_by_scope(&self.principal)
                .await?
                .models
                .into_iter()
                .map(ModelListItem::from)
                .collect())
        } else {
            self.models
                .list_models(self.principal.user.user_id.clone(), false)
                .await
        }
    }

    /// Read the current items used by discovery. Discovery intentionally does
    /// not load default/deployment metadata that model-catalog pagination does
    /// not expose. A successful read replaces the request snapshot, so a
    /// later model-dependent decision uses the generation the user just saw.
    pub async fn read_fresh_items(&self) -> CatalogResult {
        self.read(true).await
    }

    /// Read the authorized item snapshot once for model-dependent decisions in
    /// one request and its descendants. Discovery callers must use
    /// `read_fresh_items` so a cursor/revision check can observe catalog
    /// changes between pages.
    pub async fn read_snapshot(&self) -> CatalogResult {
        self.read(false).await
    }

    async fn read(&self, refresh: bool) -> CatalogResult {
        // The budget includes request/cache lock waiting, not just backend I/O.
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut snapshot = self.catalog.lock().await;
            if !refresh && let Some(items) = snapshot.as_ref() {
                return items.clone();
            }
            // Cancellation must not expose the previous successful generation.
            *snapshot = Some(Self::incomplete_read());
            let result = self.load_items(refresh).await;
            *snapshot = Some(result.clone());
            result
        })
        .await
        .unwrap_or_else(|_| Self::incomplete_read())
    }

    fn incomplete_read() -> CatalogResult {
        Err(astra_core::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Authorized model catalog read did not complete",
        ))
    }

    async fn load_items(&self, refresh: bool) -> CatalogResult {
        let cell = self.cache.snapshot_for(self).await;
        let mut cached = cell.lock().await;
        if !refresh
            && let Some(snapshot) = cached.as_ref()
            && snapshot.loaded_at.elapsed() < AuthorizedModelCatalogCache::TTL
        {
            return Ok(snapshot.items.clone());
        }
        // Invalidate before I/O so cancellation cannot retain stale success.
        *cached = None;
        let result = self.read_items().await;
        if let Ok(items) = &result
            && items.len() <= AuthorizedModelCatalogCache::MAX_ENTRY_ITEMS
            && serde_json::to_vec(items)
                .is_ok_and(|bytes| bytes.len() <= AuthorizedModelCatalogCache::MAX_ENTRY_BYTES)
        {
            *cached = Some(CatalogSnapshot {
                items: items.clone(),
                loaded_at: tokio::time::Instant::now(),
            });
        }
        result
    }

    /// Return the request's observed generation, never execution credentials
    /// or authorization. A cold execution caller still uses canonical admission.
    pub async fn cached_snapshot(&self) -> Option<Vec<ModelListItem>> {
        self.catalog
            .lock()
            .await
            .as_ref()
            .and_then(|result| result.as_ref().ok().cloned())
    }
}

impl PartialEq for AuthorizedModelCatalogReader {
    fn eq(&self, other: &Self) -> bool {
        self.principal == other.principal
            && Arc::ptr_eq(&self.models, &other.models)
            && Arc::ptr_eq(&self.auth, &other.auth)
    }
}

impl std::fmt::Debug for AuthorizedModelCatalogReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizedModelCatalogReader")
            .field("scope", &self.scope())
            .finish_non_exhaustive()
    }
}
