use super::*;
use axum::extract::rejection::JsonRejection;

#[derive(serde::Serialize)]
pub(super) struct ModelGatewayCreateResponse {
    pub id: String,
    pub status: astra_services::ModelGatewayStatus,
}

impl From<&astra_services::ModelGatewayRecord> for ModelGatewayCreateResponse {
    fn from(record: &astra_services::ModelGatewayRecord) -> Self {
        Self {
            id: record.id.clone(),
            status: record.status.clone(),
        }
    }
}

pub(super) async fn create_model_gateway_handler(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    request: Result<Json<astra_services::ModelGatewayCreateRequestData>, JsonRejection>,
) -> Result<Json<ModelGatewayCreateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let Json(request) = request.map_err(model_gateway_json_rejection_to_error)?;
    let _principal = state
        .auth_service
        .current_principal_for_request(
            &headers,
            external_request_descriptor(&method, &uri, &headers, "/model-gateways"),
        )
        .await?;
    let record = state.model_gateway_service.create_gateway(request).await?;
    Ok(Json((&record).into()))
}

fn model_gateway_json_rejection_to_error(
    rejection: JsonRejection,
) -> (StatusCode, Json<ErrorResponse>) {
    model_gateway_json_error_from_body_text(&rejection.body_text())
}

fn model_gateway_json_error_from_body_text(detail: &str) -> (StatusCode, Json<ErrorResponse>) {
    let code = if detail.contains("unknown variant") {
        "model_gateway_protocol_unsupported"
    } else {
        "model_gateway_invalid"
    };
    astra_core::error_response_coded(
        StatusCode::BAD_REQUEST,
        format!("model gateway request payload is invalid: {detail}"),
        code,
    )
}

pub(super) async fn get_model_gateway_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<astra_services::ModelGatewayRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let record = state.model_gateway_service.get_gateway(id).await?;
    Ok(Json(record))
}

pub(super) async fn disable_model_gateway_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<astra_services::ModelGatewayRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _user = state.auth_service.current_user(&headers).await?;
    let record = state.model_gateway_service.disable_gateway(id).await?;
    Ok(Json(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_gateway_unknown_protocol_json_error_maps_to_contract_code() {
        let err = model_gateway_json_error_from_body_text(
            "unknown variant `legacy`, expected `openai_chat_completions` at line 1 column 42 while parsing field model_protocol",
        );
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_protocol_unsupported")
        );
    }

    #[test]
    fn model_gateway_other_json_error_maps_to_invalid() {
        let err = model_gateway_json_error_from_body_text("missing field `resolve_url`");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("model_gateway_invalid"));
    }
}
