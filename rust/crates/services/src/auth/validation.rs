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

#[cfg(test)]
mod tests {
    use super::*;

    // --- looks_like_email ---

    #[test]
    fn email_valid() {
        assert!(looks_like_email("user@example.com"));
    }

    #[test]
    fn email_no_at() {
        assert!(!looks_like_email("userexample.com"));
    }

    #[test]
    fn email_no_dot() {
        assert!(!looks_like_email("user@example"));
    }

    #[test]
    fn email_empty() {
        assert!(!looks_like_email(""));
    }

    #[test]
    fn email_double_at() {
        assert!(!looks_like_email("user@@example.com"));
    }

    #[test]
    fn email_empty_local() {
        assert!(!looks_like_email("@example.com"));
    }

    #[test]
    fn email_at_dot() {
        assert!(!looks_like_email("@."));
    }

    // --- validate_register_request ---

    #[test]
    fn validate_valid_request() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "alice@test.com".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_ok());
    }

    #[test]
    fn validate_username_too_short() {
        let req = AuthRegisterRequestData {
            username: "ab".to_string(),
            email: "x@t.c".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_err());
    }

    #[test]
    fn validate_username_boundary_3() {
        let req = AuthRegisterRequestData {
            username: "abc".to_string(),
            email: "x@t.c".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_ok());
    }

    #[test]
    fn validate_username_invalid_chars() {
        let req = AuthRegisterRequestData {
            username: "alice!".to_string(),
            email: "x@t.c".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_err());
    }

    #[test]
    fn validate_password_too_short() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "x@t.c".to_string(),
            password: "short".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_err());
    }

    #[test]
    fn validate_password_boundary_8() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "x@t.c".to_string(),
            password: "12345678".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_ok());
    }

    #[test]
    fn validate_invalid_email() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "not-an-email".to_string(),
            password: "password123".to_string(),
            display_name: None,
        };
        assert!(validate_register_request(&req).is_err());
    }

    #[test]
    fn validate_display_name_too_long() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "x@t.c".to_string(),
            password: "password123".to_string(),
            display_name: Some("x".repeat(256)),
        };
        assert!(validate_register_request(&req).is_err());
    }

    #[test]
    fn validate_display_name_at_limit() {
        let req = AuthRegisterRequestData {
            username: "alice".to_string(),
            email: "x@t.c".to_string(),
            password: "password123".to_string(),
            display_name: Some("x".repeat(255)),
        };
        assert!(validate_register_request(&req).is_ok());
    }
}
