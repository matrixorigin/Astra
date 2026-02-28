"""Unit tests for api.sse_errors module."""

import pytest
from api.sse_errors import is_sse_endpoint, sse_error_response, status_to_error_code, format_validation_error


class TestIsSSEEndpoint:
    @pytest.mark.parametrize("path", [
        "/chat/stream",
        "/chat/runs/abc123/stream",
        "/chat/turn",
        "/streaming/chat",
    ])
    def test_sse_paths_matched(self, path):
        assert is_sse_endpoint(path) is True

    @pytest.mark.parametrize("path", [
        "/chat",
        "/chat/runs/abc123",
        "/auth/login",
        "/sessions",
        "/chat/stream/extra",
    ])
    def test_non_sse_paths_not_matched(self, path):
        assert is_sse_endpoint(path) is False


class TestStatusToErrorCode:
    @pytest.mark.parametrize("status,expected", [
        (401, "AUTH_ERROR"),
        (403, "AUTH_ERROR"),
        (404, "NOT_FOUND"),
        (422, "VALIDATION_ERROR"),
        (500, "INTERNAL_ERROR"),
        (418, "INTERNAL_ERROR"),  # unmapped → fallback
    ])
    def test_mapping(self, status, expected):
        assert status_to_error_code(status) == expected


class TestFormatValidationError:
    def test_missing_field(self):
        from fastapi.exceptions import RequestValidationError
        exc = RequestValidationError([{"type": "missing", "loc": ("body", "message"), "msg": "Field required", "input": {}}])
        assert format_validation_error(exc) == "body.message: Field required"

    def test_multiple_errors(self):
        from fastapi.exceptions import RequestValidationError
        exc = RequestValidationError([
            {"type": "missing", "loc": ("body", "a"), "msg": "Field required", "input": {}},
            {"type": "missing", "loc": ("body", "b"), "msg": "Field required", "input": {}},
        ])
        assert format_validation_error(exc) == "body.a: Field required; body.b: Field required"


class TestSSEErrorResponse:
    def test_content_type(self):
        resp = sse_error_response(401, "Not authenticated")
        assert resp.media_type == "text/event-stream"

    def test_headers(self):
        resp = sse_error_response(500, "boom")
        assert resp.headers.get("cache-control") == "no-cache"

    @pytest.mark.asyncio
    async def test_custom_code_overrides_default(self):
        resp = sse_error_response(500, "boom", code="CUSTOM")
        # Verify the body contains the custom code, not the default "INTERNAL_ERROR"
        body = "".join([chunk async for chunk in resp.body_iterator])
        import json
        event = json.loads(body.removeprefix("data: ").strip())
        assert event["code"] == "CUSTOM"
