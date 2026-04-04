use super::AuthRegisterRequestData;
use astra_core::{ErrorResponse, error_response};
use axum::{Json, http::StatusCode};

pub(super) fn validate_register_request(
    request: &AuthRegisterRequestData,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let valid_username_len = (3..=50).contains(&request.username.len());
    let valid_username_chars = request
        .username
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    let valid_password_len = (8..=72).contains(&request.password.len());
    let valid_email = looks_like_email(&request.email);
    let valid_display_name = request
        .display_name
        .as_ref()
        .map(|value| value.len() <= 255)
        .unwrap_or(true);

    let detail = if !valid_username_len {
        "username must be 3-50 characters"
    } else if !valid_username_chars {
        "username may only contain letters, digits, underscores, or hyphens"
    } else if !valid_password_len {
        "password must be 8-72 characters"
    } else if !valid_email {
        "invalid email address"
    } else if !valid_display_name {
        "display_name must be at most 255 characters"
    } else {
        return Ok(());
    };

    Err(error_response(StatusCode::UNPROCESSABLE_ENTITY, detail))
}

fn looks_like_email(value: &str) -> bool {
    let mut parts = value.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    !local.is_empty() && domain.contains('.') && parts.next().is_none()
}
