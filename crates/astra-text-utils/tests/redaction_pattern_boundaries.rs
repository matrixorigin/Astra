//! Per-pattern contract for credential redaction: every supported pattern is
//! redacted under its own label, the documented minimum lengths are exact
//! boundaries, and public syntax around the secret stays visible.
//! Formats that are not supported yet (#1170, #1171) are not asserted here.

use astra_text_utils::credential_redaction::{
    private_key_header_ranges, redact_credentials_for_display, redact_credentials_in_text,
    reject_redaction_markers_in_replacement,
};

fn display(text: &str) -> (String, usize) {
    redact_credentials_for_display(text)
}

fn label_marker(label: &str) -> String {
    format!("[REDACTED:{label}]")
}

/// Edit-capable markers carry a short code instead of the label.
fn edit_code(label: &str) -> &'static str {
    match label {
        "AWS_ACCESS_KEY" => "C1",
        "AWS_SECRET_KEY" => "C2",
        "GITHUB_TOKEN" => "C3",
        "BEARER_TOKEN" => "C4",
        "CONNECTION_CREDENTIAL" => "C5",
        "SECRET_ASSIGNMENT" => "C6",
        "TOKEN_ARGUMENT" => "C7",
        "HUGGINGFACE_TOKEN" => "C8",
        other => panic!("unknown label {other}"),
    }
}

fn assert_redacted(text: &str, secret: &str, label: &str) {
    let (out, n) = display(text);
    assert!(n >= 1, "nothing redacted in {text:?}");
    assert!(!out.contains(secret), "display leaked {secret:?}: {out}");
    assert!(
        out.contains(&label_marker(label)),
        "expected {label} marker in {out}"
    );
    let (edit, m) = redact_credentials_in_text(text);
    assert_eq!(m, n, "edit and display counts differ for {text:?}");
    assert!(!edit.contains(secret), "edit leaked {secret:?}: {edit}");
    let code = edit_code(label);
    assert!(
        edit.contains(&format!("[REDACTED:{code}:")),
        "expected {code} edit marker in {edit}"
    );
}

fn assert_untouched(text: &str) {
    let (out, n) = display(text);
    assert_eq!(
        (out.as_str(), n),
        (text, 0),
        "unexpected redaction of {text:?}"
    );
}

#[test]
fn every_supported_pattern_uses_its_own_label() {
    let cases = [
        (
            "tool --api-key abcdefghijklmnopqrstuvwx",
            "abcdefghijklmnopqrstuvwx",
            "TOKEN_ARGUMENT",
        ),
        (
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "GITHUB_TOKEN",
        ),
        (
            "mysql://admin:hunter2pass@db:3306/app",
            "hunter2pass",
            "CONNECTION_CREDENTIAL",
        ),
        (
            "password=correcthorse1",
            "correcthorse1",
            "SECRET_ASSIGNMENT",
        ),
    ];
    for (text, secret, label) in cases {
        assert_redacted(text, secret, label);
    }
    // Fixtures shaped like provider tokens are built at runtime so the source
    // file does not carry secret-shaped literals.
    let aws_key = format!("AKIA{}", "QWERTY2345ZXCVBN");
    assert_redacted(&format!("key {aws_key}"), &aws_key, "AWS_ACCESS_KEY");
    let bearer = "token-".repeat(7);
    assert_redacted(
        &format!("Authorization: Bearer {bearer}"),
        &bearer,
        "BEARER_TOKEN",
    );
    let hf = format!("hf_{}", "Ab1".repeat(12));
    assert_redacted(&format!("token {hf}"), &hf, "HUGGINGFACE_TOKEN");
    let aws_secret = format!("{}/{}", "Ab12".repeat(5), "Cd34".repeat(5));
    assert_redacted(
        &format!("aws_secret_access_key = {aws_secret}"),
        &aws_secret,
        "AWS_SECRET_KEY",
    );
}

#[test]
fn github_token_prefixes_all_match() {
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        let token = format!("{prefix}{}", "A1b2C3d4E5".repeat(4));
        assert_redacted(&format!("token {token} end"), &token, "GITHUB_TOKEN");
    }
    assert_untouched(&format!("ghx_{}", "A1b2C3d4E5".repeat(4)));
}

#[test]
fn minimum_lengths_are_exact_boundaries() {
    // (prefix, body char, minimum body length, label)
    let cases: [(&str, char, usize, &str); 6] = [
        ("ghp_", 'a', 36, "GITHUB_TOKEN"),
        ("hf_", 'a', 34, "HUGGINGFACE_TOKEN"),
        ("AKIA", 'A', 16, "AWS_ACCESS_KEY"),
        ("Bearer ", 'a', 40, "BEARER_TOKEN"),
        ("--token ", 'a', 20, "TOKEN_ARGUMENT"),
        ("password=", 'a', 12, "SECRET_ASSIGNMENT"),
    ];
    for (prefix, ch, min, label) in cases {
        let short = format!("{prefix}{}", ch.to_string().repeat(min - 1));
        assert_untouched(&short);
        let body = ch.to_string().repeat(min);
        assert_redacted(&format!("{prefix}{body}"), &body, label);
    }
    let short_secret = format!("aws_secret_access_key={}", "a".repeat(29));
    let (out, _) = display(&short_secret);
    assert!(!out.contains("[REDACTED:AWS_SECRET_KEY]"), "{out}");
    let secret = "a".repeat(30);
    assert_redacted(
        &format!("aws_secret_access_key={secret}"),
        &secret,
        "AWS_SECRET_KEY",
    );
}

#[test]
fn assignment_keys_and_separators_are_case_insensitive() {
    for key in [
        "password",
        "PASSWORD",
        "Passwd",
        "api_key",
        "APIKEY",
        "access_token",
        "auth_token",
        "secret_key",
        "SECRET_ACCESS_KEY",
    ] {
        for sep in ["=", ": ", " = ", ":"] {
            let text = format!("{key}{sep}abcdefghijkl");
            assert_redacted(&text, "abcdefghijkl", "SECRET_ASSIGNMENT");
            let (out, _) = display(&text);
            assert!(out.starts_with(key), "key text must stay visible: {out}");
        }
    }
    for text in [
        "\"password\": \"abcdefghijkl\"",
        "config['api_key'] = 'abcdefghijkl'",
        "[\"auth_token\"]=abcdefghijkl",
    ] {
        assert_redacted(text, "abcdefghijkl", "SECRET_ASSIGNMENT");
    }
}

#[test]
fn token_argument_flag_variants() {
    for flag in [
        "--token",
        "-token",
        "--api-key",
        "--api_key",
        "--apikey",
        "--access-token",
        "--auth_token",
        "--TOKEN",
    ] {
        for sep in [" ", "="] {
            let text = format!("cli {flag}{sep}abcdefghijklmnopqrstu next");
            assert_redacted(&text, "abcdefghijklmnopqrstu", "TOKEN_ARGUMENT");
            let (out, _) = display(&text);
            assert!(out.contains(flag) && out.ends_with(" next"), "{out}");
        }
    }
}

#[test]
fn every_occurrence_is_counted() {
    let key_a = format!("AKIA{}", "A".repeat(16));
    let key_b = format!("AKIA{}", "B".repeat(16));
    let text = format!(
        "a={key_a} b={key_b}\npassword=abcdefghijkl\nghp_abcdefghijklmnopqrstuvwxyz0123456789"
    );
    let text = text.as_str();
    let (out, n) = display(text);
    assert_eq!(n, 4, "{out}");
    assert_eq!(out.matches("[REDACTED:AWS_ACCESS_KEY]").count(), 2);
    assert_eq!(out.matches("[REDACTED:SECRET_ASSIGNMENT]").count(), 1);
    assert_eq!(out.matches("[REDACTED:GITHUB_TOKEN]").count(), 1);
}

#[test]
fn benign_text_is_untouched() {
    for text in [
        "",
        "plain log line with no secrets",
        "password=short",
        "the password is configured elsewhere",
        "https://example.com/path?x=1",
        "ssh://host:22/repo",
        "Bearer of bad news",
        "AKIA is an AWS prefix",
        "中文日志 🚀 没有凭据",
    ] {
        assert_untouched(text);
    }
}

#[test]
fn redaction_preserves_surrounding_text_and_lines() {
    let text = "line1\n部署 password=abcdefghijkl 完成\nline3";
    let (out, n) = display(text);
    assert_eq!(n, 1);
    assert_eq!(out.lines().count(), 3);
    assert!(out.starts_with("line1\n部署 password="), "{out}");
    assert!(out.ends_with(" 完成\nline3"), "{out}");
}

#[test]
fn private_key_header_ranges_find_each_header() {
    let text = "a\n-----BEGIN RSA PRIVATE KEY-----\nx\n-----END RSA PRIVATE KEY-----\nb\n-----BEGIN OPENSSH PRIVATE KEY-----\n";
    let ranges = private_key_header_ranges(text);
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    for (start, end) in ranges {
        assert!(text[start..end].starts_with("-----BEGIN "));
        assert!(text[start..end].ends_with("PRIVATE KEY-----"));
    }
    assert!(private_key_header_ranges("-----BEGIN PUBLIC KEY-----").is_empty());
}

#[test]
fn replacement_text_with_any_marker_is_rejected() {
    let (edit, _) = redact_credentials_in_text("password=abcdefghijkl");
    let (shown, _) = display("password=abcdefghijkl");
    assert!(reject_redaction_markers_in_replacement(&edit).is_err());
    assert!(reject_redaction_markers_in_replacement(&shown).is_err());
    assert!(reject_redaction_markers_in_replacement("[REDACTED:CUSTOM_LABEL]").is_err());
    assert!(reject_redaction_markers_in_replacement("password=<your-db-password>").is_ok());
    assert!(reject_redaction_markers_in_replacement("ordinary text [not a marker]").is_ok());
}
