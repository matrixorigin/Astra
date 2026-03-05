"""Prometheus metrics for monitoring."""

from fastapi import Response
from prometheus_client import CONTENT_TYPE_LATEST, Counter, Gauge, Histogram, generate_latest

from config.settings import get_settings

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

# Intent routing metrics (docs/design/token-efficient-llm-routing.md §Monitoring)
routing_efficiency_ratio = Histogram(
    "routing_efficiency_ratio",
    "1 - (routed_tokens / full_tokens), target > 0.45",
    buckets=[0.0, 0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0],
)

routing_confidence = Histogram(
    "routing_confidence",
    "Routing confidence score, target avg > 0.88",
    buckets=[0.0, 0.5, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0],
)

routing_fallback_total = Counter(
    "routing_fallback_total", "Routing fallbacks to full context, target rate < 2%"
)

intent_correction_total = Counter(
    "intent_correction_total", "User correction overrides, target rate < 0.8%"
)

routing_cache_hit_total = Counter(
    "routing_cache_hit_total", "Tier 0 high-confidence hits (skip Tier 1), target > 75%"
)

routing_requests_total = Counter(
    "routing_requests_total", "Total routing requests (denominator for rates)"
)

adaptive_threshold_value = Gauge(
    "adaptive_threshold_value", "Current adaptive threshold, target 0.80-0.90"
)


async def metrics_endpoint() -> Response:
    """Prometheus metrics endpoint."""
    return Response(content=generate_latest(), media_type=CONTENT_TYPE_LATEST)
