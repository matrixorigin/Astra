//! One authenticated catalog boundary shared by HTTP and on-demand observation.
//! Binding is inert: `read_snapshot` loads only the authorized model items
//! once for internal admission, while `read_fresh_items` refreshes the same
//! request-scoped lookup for paginated discovery. Neither is an execution
//! grant.

use std::sync::Arc;

use astra_core::ErrorResponse;
use axum::{Json, http::StatusCode};

use super::{ModelListItem, ModelService, UserModelCatalog};
use crate::auth::{AuthPrincipal, AuthService};

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
    catalog: Arc<tokio::sync::Mutex<Option<Vec<ModelListItem>>>>,
}

impl AuthorizedModelCatalogReader {
    pub fn new(
        models: Arc<dyn ModelService>,
        auth: Arc<dyn AuthService>,
        principal: AuthPrincipal,
    ) -> Self {
        Self {
            models,
            auth,
            principal,
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

    async fn read_items(&self) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
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
    pub async fn read_fresh_items(
        &self,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let mut snapshot = self.catalog.lock().await;
        let items = self.read_items().await?;
        *snapshot = Some(items.clone());
        Ok(items)
    }

    /// Read the authorized item snapshot once for model-dependent decisions in
    /// one request and its descendants. Discovery callers must use
    /// `read_fresh_items` so a cursor/revision check can observe catalog
    /// changes between pages.
    pub async fn read_snapshot(
        &self,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<ErrorResponse>)> {
        let mut snapshot = self.catalog.lock().await;
        if let Some(items) = snapshot.as_ref() {
            return Ok(items.clone());
        }
        let items = self.read_items().await?;
        *snapshot = Some(items.clone());
        Ok(items)
    }

    /// Return the current snapshot only when another admission or discovery
    /// operation has already loaded it. A cold caller must use the model
    /// service's canonical admission path so user/Genesis authorization is
    /// not performed twice.
    pub async fn cached_snapshot(&self) -> Option<Vec<ModelListItem>> {
        self.catalog.lock().await.clone()
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
