"""SSE protocol-compliant error responses.

SSE endpoints must ALWAYS return Content-Type: text/event-stream, even on errors.
Errors are conveyed as SSE events with unified format:
    {"type": "error", "message": "...", "code": "...", "retryable": false}

Two layers ensure compliance:
1. Exception handlers in main.py — catch auth/validation errors raised by FastAPI
   dependency injection (Depends(get_current_user), Pydantic) BEFORE the handler runs.
2. Generator-internal try/except — catch errors DURING streaming (session lookup,
   LLM failures, DB errors, etc.).
"""

import json
import re

from fastapi.responses import StreamingResponse

# Paths that return SSE streams — errors on these must also be SSE.
_SSE_PATH_PATTERNS: list[re.Pattern] = [
    re.compile(r"^/chat/stream$"),
    re.compile(r"^/chat/runs/[^/]+/stream$"),
    re.compile(r"^/chat/turn$"),
    re.compile(r"^/streaming/chat$"),
]

SSE_HEADERS: dict[str, str] = {
    "Cache-Control": "no-cache",
    "Connection": "keep-alive",
    "X-Accel-Buffering": "no",
}

_STATUS_TO_CODE: dict[int, str] = {
    401: "AUTH_ERROR",
    403: "AUTH_ERROR",
    404: "NOT_FOUND",
    422: "VALIDATION_ERROR",
}


def is_sse_endpoint(path: str) -> bool:
    return any(p.match(path) for p in _SSE_PATH_PATTERNS)


def status_to_error_code(status_code: int) -> str:
    """Map HTTP status code to SSE error code. Single source of truth."""
    return _STATUS_TO_CODE.get(status_code, "INTERNAL_ERROR")


def format_validation_error(exc) -> str:
    """Summarize a RequestValidationError for SSE clients.

    Produces a concise, client-friendly message like "message: Field required"
    instead of leaking raw Pydantic internals and pydantic.dev URLs.
    """
    parts = [
        f"{'.'.join(str(loc_item) for loc_item in e['loc'])}: {e['msg']}" for e in exc.errors()
    ]
    return "; ".join(parts) or "Validation error"


def sse_error_response(
    status_code: int, message: str, code: str | None = None, retryable: bool = False
) -> StreamingResponse:
    """Return a StreamingResponse with a single SSE error event."""
    if code is None:
        code = status_to_error_code(status_code)
    event = {"type": "error", "message": message, "code": code, "retryable": retryable}
    body = f"data: {json.dumps(event)}\n\n"
    return StreamingResponse(
        iter([body]),
        media_type="text/event-stream",
        headers=SSE_HEADERS,
    )
