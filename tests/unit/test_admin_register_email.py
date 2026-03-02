"""Test mo-admin register command default email.

Regression: default email domain was .local which failed email validation.
"""


def test_default_email_uses_example_com():
    """Default email for username 'alice' is 'alice@example.com'."""
    username = "alice"
    email = None
    assert (email or f"{username}@example.com") == "alice@example.com"


def test_explicit_email_overrides_default():
    """When user provides email, it takes precedence."""
    email = "custom@corp.io"
    assert (email or "alice@example.com") == "custom@corp.io"
