//! One authenticated catalog boundary shared by HTTP and on-demand observation.
//! Binding is inert: `read_snapshot` loads one request-scoped catalog snapshot
//! for internal admission, while `read` keeps fresh discovery semantics for
//! paginated model-catalog calls. Neither is an execution grant.

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
    if principal.is_edge_registration() {
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
    catalog: Arc<tokio::sync::OnceCell<UserModelCatalog>>,
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
            catalog: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.principal.user.user_id
    }

    pub fn scope(&self) -> &'static str {
        if self.principal.is_edge_registration() {
            "edge_registration"
        } else {
            "user"
        }
    }

    pub async fn read(&self) -> Result<UserModelCatalog, (StatusCode, Json<ErrorResponse>)> {
        read_authorized_model_catalog(self.models.as_ref(), self.auth.as_ref(), &self.principal)
            .await
    }

    /// Read the authorized catalog once for model-dependent decisions in one
    /// request and its descendants. Discovery callers must use `read` so a
    /// cursor/revision check can observe catalog changes between pages.
    pub async fn read_snapshot(
        &self,
    ) -> Result<UserModelCatalog, (StatusCode, Json<ErrorResponse>)> {
        self.catalog
            .get_or_try_init(|| async {
                read_authorized_model_catalog(
                    self.models.as_ref(),
                    self.auth.as_ref(),
                    &self.principal,
                )
                .await
            })
            .await
            .cloned()
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
