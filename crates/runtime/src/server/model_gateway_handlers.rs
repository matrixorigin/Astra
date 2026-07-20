use super::*;

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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelGatewayCreateWireRequest {
    id: String,
    resolve_url: String,
    model_protocol: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

fn decode_model_gateway_create_request(
    body: &[u8],
) -> Result<astra_services::ModelGatewayCreateRequestData, (StatusCode, Json<ErrorResponse>)> {
    let wire = serde_json::from_slice::<ModelGatewayCreateWireRequest>(body).map_err(|error| {
        astra_core::error_response_coded(
            StatusCode::BAD_REQUEST,
            format!("model gateway request payload is invalid: {error}"),
            "model_gateway_invalid",
        )
    })?;
    Ok(astra_services::ModelGatewayCreateRequestData {
        id: wire.id,
        resolve_url: wire.resolve_url,
        model_protocol: astra_services::ModelProtocol::from_wire_value(&wire.model_protocol)?,
        metadata: wire.metadata,
    })
}

pub(super) async fn create_model_gateway_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<ModelGatewayCreateResponse>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let request = decode_model_gateway_create_request(&body)?;
    let record = state.model_gateway_service.create_gateway(request).await?;
    Ok(Json((&record).into()))
}

pub(super) async fn get_model_gateway_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<astra_services::ModelGatewayRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let record = state.model_gateway_service.get_gateway(id).await?;
    Ok(Json(record))
}

pub(super) async fn disable_model_gateway_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<astra_services::ModelGatewayRecord>, (StatusCode, Json<ErrorResponse>)> {
    let _admin = state.admin.authorizer.require_admin(&headers).await?;
    let record = state.model_gateway_service.disable_gateway(id).await?;
    Ok(Json(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_gateway_unknown_protocol_has_typed_contract_error() {
        let err = decode_model_gateway_create_request(
            br#"{"id":"gateway-a","resolve_url":"https://gateway.example/v1","model_protocol":"legacy"}"#,
        )
        .expect_err("unsupported protocol");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1.error_code.as_deref(),
            Some("model_gateway_protocol_unsupported")
        );
    }

    #[test]
    fn malformed_model_gateway_payload_has_shape_error() {
        let err = decode_model_gateway_create_request(
            br#"{"id":"gateway-a","model_protocol":"openai_chat_completions"}"#,
        )
        .expect_err("missing endpoint");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1.error_code.as_deref(), Some("model_gateway_invalid"));
    }

    #[test]
    fn model_gateway_wire_request_decodes_without_transport_fields_leaking() {
        let request = decode_model_gateway_create_request(
            br#"{"id":"gateway-a","resolve_url":"https://gateway.example/v1","model_protocol":"openai_chat_completions","metadata":{"region":"cn"}}"#,
        )
        .expect("valid request");
        assert_eq!(request.id, "gateway-a");
        assert_eq!(request.resolve_url, "https://gateway.example/v1");
        assert_eq!(
            request.model_protocol,
            astra_services::ModelProtocol::OpenAiChatCompletions
        );
        assert_eq!(request.metadata, Some(serde_json::json!({"region": "cn"})));
    }
}
