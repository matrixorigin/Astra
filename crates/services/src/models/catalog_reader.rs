//! One authenticated catalog boundary shared by HTTP and on-demand observation.
//! Binding is inert: only `read` loads a catalog. It is never an execution grant.

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
