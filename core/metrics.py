"""Prometheus metrics for monitoring."""

from fastapi import Response
from prometheus_client import CONTENT_TYPE_LATEST, Counter, Gauge, Histogram, generate_latest

from core.config import get_settings

settings = get_settings()

# Request metrics
http_requests_total = Counter(
    "http_requests_total", "Total HTTP requests", ["method", "endpoint", "status"]
)

http_request_duration_seconds = Histogram(
    "http_request_duration_seconds", "HTTP request duration in seconds", ["method", "endpoint"]
)

# Skill metrics
skill_executions_total = Counter(
    "skill_executions_total", "Total skill executions", ["skill_name", "status"]
)

skill_execution_duration_seconds = Histogram(
    "skill_execution_duration_seconds", "Skill execution duration in seconds", ["skill_name"]
)

# LLM metrics
llm_calls_total = Counter("llm_calls_total", "Total LLM API calls", ["provider", "model", "status"])

llm_tokens_total = Counter(
    "llm_tokens_total",
    "Total LLM tokens used",
    ["provider", "model", "type"],  # prompt or completion
)

llm_cost_usd_total = Counter("llm_cost_usd_total", "Total LLM cost in USD", ["provider", "model"])

# Database metrics
db_queries_total = Counter("db_queries_total", "Total database queries", ["operation", "status"])

db_query_duration_seconds = Histogram(
    "db_query_duration_seconds", "Database query duration in seconds", ["operation"]
)

# System metrics
active_sessions = Gauge("active_sessions", "Number of active sessions")

rate_limit_exceeded_total = Counter(
    "rate_limit_exceeded_total",
    "Total rate limit exceeded events",
    ["key_type"],  # user or ip
)


async def metrics_endpoint() -> Response:
    """Prometheus metrics endpoint."""
    return Response(content=generate_latest(), media_type=CONTENT_TYPE_LATEST)
