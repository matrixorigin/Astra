"""LLM client with provider abstraction, routing, rate limiting, circuit breaker, and call logging."""

import asyncio
import json
import logging
import os
import re
import time
from contextlib import contextmanager
from contextvars import ContextVar
from datetime import datetime, timezone

from sqlalchemy import text
from uuid_utils import uuid7

from api.database import get_db_session
from core.llm.models import LLMCallLog, LLMMessage, LLMProvider, LLMResponse
from core.llm.providers import AnthropicProvider, BaseProvider, GroqProvider, OpenAIProvider
from core.llm.rate_limiter import RateLimiter
from core.llm.router import ModelConfig, ModelRouter
from core.utils.id_generator import generate_tool_call_id
from core.auth.encryption import decrypt_token
from core.db_consumer import DbConsumer, DbFactory

logger = logging.getLogger(__name__)

# Sentinel for asyncio.to_thread(next, iter, _END) — distinct from StopIteration.
_END = object()


def _response_guard_fps(messages: list) -> list[str] | None:
    """Build fingerprints from messages for response guard (lightweight)."""
    from core.llm.response_guard import build_fingerprints
    # Normalize LLMMessage objects to dicts before fingerprinting.
    dicts = []
    for m in messages:
        if isinstance(m, dict):
            dicts.append(m)
        elif hasattr(m, "role"):
            d: dict = {"role": m.role}
            if m.content is not None:
                d["content"] = m.content
            dicts.append(d)
    return build_fingerprints(dicts) or None


class BudgetExceededError(Exception):
    """Raised when estimated cost exceeds remaining budget."""

    pass


class ContextOverflowError(Exception):
    """Raised when prompt tokens would exceed model's context window."""

    pass


_PROVIDER_DEFAULT_MODELS = {
    "openai": "gpt-4o",
    "deepseek": "deepseek-chat",
    "anthropic": "claude-3-5-sonnet-20241022",
    "groq": "llama3-70b",
}


def _default_model_for_provider(provider: str) -> str:
    return _PROVIDER_DEFAULT_MODELS.get(provider, "gpt-4o")


_STANDARD_TC_ID = re.compile(r"^call_[a-zA-Z0-9]+$")

# HTTP 4xx status codes that are client errors (our fault, not server's).
# These should NOT trigger circuit breaker — retrying won't help.
# 429 is excluded: it's rate limiting, which IS retryable.
_CLIENT_ERROR_CODES = {400, 401, 403, 404, 405, 409, 413, 415, 422}
_CLIENT_ERROR_PATTERN = re.compile(r"Error code: (4\d{2})\b")


def _is_client_error(error: Exception) -> bool:
    """Return True if error is a client-side 4xx error (not retryable).

    Client errors (bad parameters, auth failures) are our fault — the circuit
    breaker should not penalize the provider for them.  429 (rate limit) is
    excluded because it IS retryable after backoff.
    """
    status = getattr(error, "status_code", None)
    if isinstance(status, int) and status in _CLIENT_ERROR_CODES:
        return True
    # Some SDKs embed status in the error message
    m = _CLIENT_ERROR_PATTERN.search(str(error))
    if m and int(m.group(1)) in _CLIENT_ERROR_CODES:
        return True
    return False


def _rewrite_tool_call_ids(messages: list[dict]) -> list[dict]:
    """Rewrite non-standard tool_call_ids to OpenAI-compatible format.

    Models with strict_tool_call_ids quirk (e.g. kimi-k2.5) reject ids like
    "read_file:1". This rewrites any id not matching "call_xxx" to a
    deterministic "call_<uuid>" and keeps assistant/tool messages consistent.
    """
    id_map: dict[str, str] = {}

    def _map(old_id: str) -> str:
        if not old_id or _STANDARD_TC_ID.match(old_id):
            return old_id
        if old_id not in id_map:
            id_map[old_id] = generate_tool_call_id()
        return id_map[old_id]

    out: list[dict] = []
    for msg in messages:
        role = msg.get("role")
        if role == "assistant" and msg.get("tool_calls"):
            msg = dict(msg)
            msg["tool_calls"] = [
                {**tc, "id": _map(tc.get("id", ""))} for tc in msg["tool_calls"]
            ]
        elif role == "tool" and msg.get("tool_call_id"):
            new_id = _map(msg["tool_call_id"])
            if new_id != msg["tool_call_id"]:
                msg = {**msg, "tool_call_id": new_id}
        out.append(msg)
    return out


class LLMClient(DbConsumer):
    """LLM client with routing, rate limiting, circuit breaker, budget control, and logging.

    Thread/async-safety: expensive state (providers, rate_limiter, config) is
    shared.  Per-request user context (user_id, router) lives in a ContextVar
    so concurrent async generators in the same event loop never interfere.
    Use ``request_context(user_id)`` to bind per-request state.
    """

    # Per-request overrides — invisible across coroutines / threads.
    _ctx_user_id: ContextVar[str | None] = ContextVar("_ctx_user_id", default=None)
    _ctx_router: ContextVar[ModelRouter | None] = ContextVar("_ctx_router", default=None)

    def __init__(
        self,
        db_factory: DbFactory,
        user_id: str | None = None,
        scope_context: dict | None = None,  # kept for backward compat, unused
    ) -> None:
        """Initialize LLM client. Models must be registered in database."""
        super().__init__(db_factory)
        self.user_id = user_id

        self._providers: dict[str, BaseProvider] = {}
        self._model_keys: dict[str, str] = {}  # model_name -> decrypted api_key
        with self._db() as db:
            self.router = ModelRouter(db=db, user_id=user_id)
        self.rate_limiter = RateLimiter()
        self._load_config()
        self._init_providers()
        self._init_rate_limits()

    # ── Per-request context (concurrency-safe) ─────────────────────

    # Auxiliary LLM call tracker — collects stats for non-chat calls
    # (memory extraction, verification, etc.) within a turn for EXPLAIN.
    _ctx_aux_calls: ContextVar[list[dict] | None] = ContextVar(
        "_ctx_aux_calls", default=None,
    )

    @contextmanager
    def track_auxiliary_calls(self):
        """Context manager that collects auxiliary LLM call stats for EXPLAIN."""
        calls: list[dict] = []
        tok = self._ctx_aux_calls.set(calls)
        try:
            yield calls
        finally:
            self._ctx_aux_calls.reset(tok)

    def _record_auxiliary(self, task_hint: str, tokens_prompt: int, tokens_completion: int,
                          cost_usd: float, latency_ms: int) -> None:
        bucket = self._ctx_aux_calls.get()
        if bucket is not None:
            bucket.append({
                "purpose": task_hint,
                "tokens_in": tokens_prompt,
                "tokens_out": tokens_completion,
                "cost_usd": round(cost_usd, 6),
                "ms": latency_ms,
            })

    @contextmanager
    def request_context(self, user_id: str | None = None):
        """Bind user_id + router for the current execution context.

        Safe for concurrent use: each coroutine / thread sees its own
        values via ContextVar, so two parallel SSE generators never
        overwrite each other's user context.
        """
        with self._db() as db:
            router = ModelRouter(db=db, user_id=user_id)
        tok_uid = self._ctx_user_id.set(user_id)
        tok_rtr = self._ctx_router.set(router)
        try:
            yield
        finally:
            self._ctx_user_id.reset(tok_uid)
            self._ctx_router.reset(tok_rtr)

    @property
    def _active_user_id(self) -> str | None:
        """Return per-request user_id if set, else fall back to instance default."""
        return self._ctx_user_id.get() or self.user_id

    @property
    def _active_router(self) -> ModelRouter:
        """Return per-request router if set, else fall back to instance default."""
        return self._ctx_router.get() or self.router

    # ── Config (#4 动态配置) ───────────────────────────────────────

    def _load_config(self) -> None:
        """Load config: DB → env → auto-detect from registered tokens."""
        with self._db() as db:
            config = None
            try:
                from api.models import Config
                row = db.query(Config.value).filter(Config.key_name == "llm_config").first()
                if row:
                    config = json.loads(row[0])
            except Exception:
                pass
            if not config:
                provider = os.getenv("LLM_PROVIDER", "")
                model = os.getenv("LLM_MODEL", "")
                if not provider or not model:
                    try:
                        from api.models import LLMModel
                        row = db.query(LLMModel.model_name, LLMModel.provider).filter(
                            LLMModel.is_active == 1
                        ).order_by(LLMModel.created_at).first()
                        if row:
                            model = model or row[0]
                            provider = provider or row[1]
                    except Exception:
                        pass
                provider = provider or "openai"
                if not model:
                    model = _default_model_for_provider(provider)
                config = {
                    "provider": provider,
                    "model": model,
                    "temperature": float(os.getenv("LLM_TEMPERATURE", "0.7")),
                    "max_tokens": int(os.getenv("LLM_MAX_TOKENS", "8192")),
                    "budget_usd": float(os.getenv("LLM_BUDGET_USD", "0")),
                }
            self.config = config
            self._validate_config()
            self._total_spend_usd = 0.0

    def _validate_config(self) -> None:
        """Validate config values; raise ValueError on invalid."""
        budget = self.config.get("budget_usd", 0)
        if budget < 0:
            raise ValueError(f"budget_usd must be >= 0, got {budget}")
        temp = self.config.get("temperature", 0.7)
        if not 0 <= temp <= 2:
            raise ValueError(f"temperature must be 0-2, got {temp}")
        max_tok = self.config.get("max_tokens")
        if max_tok is not None and max_tok <= 0:
            raise ValueError(f"max_tokens must be > 0, got {max_tok}")

    def reload_config(self):
        """Hot reload config + model registry (#4)."""
        self._load_config()
        with self._db() as db:
            self.router.reload(db)
        self._init_rate_limits()
        logger.info("LLM config reloaded")

    # ── Provider init (#3 异构) ────────────────────────────────────

    def _init_providers(self) -> None:
        """Initialize provider clients from infra_llm_models table (active models only)."""
        # Init built-in mock provider (no key needed, always available)
        with self._db() as db:
            try:
                from core.llm.providers import MockEchoProvider
                self._providers["mock"] = MockEchoProvider()
                self.router.register(ModelConfig(
                    model_name="mock-echo", provider="mock",
                    tags=["test", "builtin"],
                ))
            except Exception:
                pass

            # Load active models and init their providers (per-model keys)
            try:
                from api.models import LLMModel
                rows = db.query(LLMModel).filter(LLMModel.is_active == 1).all()
            except Exception as e:
                logger.debug(f"Failed to load infra_llm_models: {e}")
                return

            for row in rows:
                provider_name = row.provider
                model_name = row.model_name
                try:
                    api_key = decrypt_token(row.api_key_encrypted)
                except Exception as e:
                    logger.warning(f"Failed to decrypt key for {model_name}: {e}")
                    continue
                self._model_keys[model_name] = api_key

                # Create provider instance per model (not per provider name).
                # This allows multiple models from same provider with different base_urls.
                # Skip if a built-in provider is already registered under the plain
                # provider name (e.g. "mock" → MockEchoProvider) — the built-in
                # takes precedence over DB-created OpenAIProvider instances.
                provider_key = f"{provider_name}:{model_name}"
                if provider_key in self._providers:
                    continue
                if provider_name in self._providers:
                    # Built-in provider exists — reuse it for this model
                    self._providers[provider_key] = self._providers[provider_name]
                    logger.debug(f"Reusing built-in {provider_name} provider for {model_name}")
                    continue
                try:
                    base_url = row.base_url
                    if provider_name == "groq" and not base_url:
                        self._providers[provider_key] = GroqProvider(api_key)
                    elif provider_name == "anthropic" and not base_url:
                        self._providers[provider_key] = AnthropicProvider(api_key)
                    else:
                        kwargs = {"base_url": base_url} if base_url else {}
                        self._providers[provider_key] = OpenAIProvider(api_key, **kwargs)
                    logger.debug(f"Initialized {provider_name} provider for {model_name}")
                except Exception as e:
                    hint = ""
                    if "No module named" in str(e):
                        mod = str(e).split("'")[1] if "'" in str(e) else "unknown"
                        hint = f" — fix: pip install {mod}"
                    logger.warning(f"Failed to initialize {provider_name} provider: {e}{hint}")

    def _get_provider_base_url(self, provider: str) -> str | None:
        """Get base_url from infra_llm_models table."""
        with self._db() as db:
            from core.llm.constants import PROVIDER_BASE_URLS
            try:
                from api.models import LLMModel
                row = db.query(LLMModel.base_url).filter(
                    LLMModel.provider == provider, LLMModel.is_active == 1
                ).first()
                if row and row[0]:
                    return row[0]
            except Exception:
                pass
            return PROVIDER_BASE_URLS.get(provider)

    def _get_api_key(self, provider: str, model_name: str | None = None) -> str | None:
        """Get API key — prefer per-model key, fall back to first key for same provider."""
        if model_name and model_name in self._model_keys:
            return self._model_keys[model_name]
        # Fall back to first key whose model belongs to the requested provider
        for mname, key in self._model_keys.items():
            cfg = self.router.registry.get(mname)
            if cfg and str(cfg.provider) == provider:
                return key
        return None

    def _init_rate_limits(self) -> None:
        for m in self.router.list_models():
            self.rate_limiter.configure(m.model_name, m.rpm_limit, m.tpm_limit)

    def _get_provider(self, p, model_name: str | None = None) -> BaseProvider:
        name = p.value if isinstance(p, LLMProvider) else str(p)
        
        # Try model-specific provider first (format: "provider:model").
        # _refresh_providers always stores as "provider:model", so this is
        # the primary lookup path for DB-registered models.
        if model_name:
            provider_key = f"{name}:{model_name}"
            provider = self._providers.get(provider_key)
            if provider:
                return provider
        
        # Fall back to plain provider name — used by built-in providers
        # (e.g. "mock") that are registered without a model-specific key.
        provider = self._providers.get(name)
        if provider:
            return provider
        
        # Not cached — try lazy init from DB.
        provider = self._lazy_init_provider(name, model_name)
        if provider:
            return provider
        
        # Check if model is registered in database
        with self._db() as db:
            from api.models import LLMModel
            registered = db.query(LLMModel).filter(
                LLMModel.provider == name, LLMModel.is_active == 1
            ).first()
        
        if registered:
            raise ValueError(
                f"Provider '{name}' is registered but failed to initialize. "
                f"Check logs or try: make dev-api-restart"
            )
        else:
            raise ValueError(
                f"Provider '{name}' is not available. "
                f"Check: pip install openai  and  mo-admin model check <model>"
            )
    
    def _lazy_init_provider(self, provider_name: str, model_name: str | None = None) -> BaseProvider | None:
        """Attempt to initialize a provider on-demand from database.

        Always stores with "provider:model" key when model_name is known,
        matching the key format used by _refresh_providers.
        """
        try:
            with self._db() as db:
                from api.models import LLMModel
                if model_name:
                    row = db.query(LLMModel).filter(
                        LLMModel.model_name == model_name, LLMModel.is_active == 1
                    ).first()
                else:
                    row = db.query(LLMModel).filter(
                        LLMModel.provider == provider_name, LLMModel.is_active == 1
                    ).first()
                
                if not row:
                    return None
                
                api_key = decrypt_token(row.api_key_encrypted)
                base_url = row.base_url
                resolved_model = model_name or row.model_name
                
                if provider_name == "groq" and not base_url:
                    provider = GroqProvider(api_key)
                elif provider_name == "anthropic" and not base_url:
                    provider = AnthropicProvider(api_key)
                else:
                    kwargs = {"base_url": base_url} if base_url else {}
                    provider = OpenAIProvider(api_key, **kwargs)
                
                # Always use "provider:model" key for consistency with _refresh_providers
                provider_key = f"{provider_name}:{resolved_model}"
                self._providers[provider_key] = provider
                logger.info(f"Lazy-initialized {provider_name} provider for {resolved_model}")
                return provider
        except Exception as e:
            logger.warning(f"Failed to lazy-init {provider_name}: {e}")
            return None

    def _resolve_model(self, model: str | None) -> str:
        from core.llm.model_resolver import resolve_model
        return resolve_model(request_model=model, default_model=self.config.get("model", "gpt-4o"))

    def resolve_model_name(self, model: str | None = None) -> str:
        """Public accessor: return the model name that would be used for a request."""
        return self._resolve_model(model)

    # ── Budget control (#7) ────────────────────────────────────────

    def _check_budget(self, model: str, messages: list | None = None):
        budget = self.config.get("budget_usd", 0)
        if budget <= 0:
            return  # unlimited
        estimated_tokens = 1000
        if messages:
            # ~4 chars per token, rough but better than fixed 1000
            char_count = sum(len(m.get("content", "") or "") for m in messages if isinstance(m, dict))
            estimated_tokens = max(char_count // 4, 200)
        estimated_cost = self._active_router.estimate_cost(model, estimated_tokens)
        if self._total_spend_usd + estimated_cost > budget:
            raise BudgetExceededError(
                f"Estimated cost ${estimated_cost:.4f} would exceed budget "
                f"(spent ${self._total_spend_usd:.4f} of ${budget:.2f})"
            )

    def _check_context_overflow(self, model: str, messages: list | None = None):
        """Check if messages would overflow the model's context window.

        Raises ContextOverflowError if estimated prompt tokens exceed context window,
        which would cause API errors like 'max_tokens must be at least 1, got -31608'.
        """
        if not messages:
            return

        # Estimate prompt tokens (~4 chars per token for ASCII, ~1 for CJK)
        char_count = 0
        for m in messages:
            if isinstance(m, dict):
                content = m.get("content", "") or ""
                char_count += len(content)
                # Tool calls can be large
                if m.get("tool_calls"):
                    char_count += len(str(m["tool_calls"]))

        # Conservative estimate: ASCII ~4 chars/token, but CJK ~1 char/token
        # Use 3 as middle ground to avoid underestimating
        estimated_tokens = char_count // 3

        # Get model's context window (default 128K if unknown)
        context_window = 128000
        for cfg in self._active_router.list_models():
            if cfg.model_name == model:
                context_window = cfg.context_window or 128000
                break

        # Reserve at least 1000 tokens for response
        max_prompt_tokens = context_window - 1000

        if estimated_tokens > max_prompt_tokens:
            raise ContextOverflowError(
                f"Context overflow: estimated {estimated_tokens:,} prompt tokens "
                f"exceeds model's context window ({context_window:,} tokens). "
                f"Start a new session with /session new to clear history."
            )

    def _record_spend(self, cost: float):
        self._total_spend_usd += cost

    # ── Core dispatch with circuit breaker (#6) ────────────────────

    def _check_model_permission(self, model: str):
        """Check if current user has permission to use the model."""
        available_models = self._active_router.list_models()
        model_names = [m.model_name for m in available_models]

        if model not in model_names:
            # Build detailed error message
            scope_info = []
            if self._active_user_id:
                scope_info.append(f"user '{self._active_user_id}'")

            scope_str = " and ".join(scope_info) if scope_info else "current scope"

            error_msg = [
                f"Model '{model}' is not available for {scope_str}.",
                f"\nAvailable models ({len(model_names)}):",
            ]

            # Group by provider for better readability
            by_provider: dict[str, list[str]] = {}
            for m in available_models:
                provider = m.provider.value if isinstance(m.provider, LLMProvider) else str(m.provider)
                if provider not in by_provider:
                    by_provider[provider] = []
                by_provider[provider].append(m.model_name)

            for provider, models in sorted(by_provider.items()):
                error_msg.append(f"  {provider}: {', '.join(models)}")

            if not model_names:
                error_msg.append("\nNo models configured. Please contact your administrator.")

            raise PermissionError("\n".join(error_msg))

    def _resolve_chain(self, model: str, task_hint: str | None = None) -> list[ModelConfig]:
        """Permission check + route to model chain. Used by _dispatch and streaming methods."""
        self._check_model_permission(model)
        chain = self._active_router.route(model, task_hint=task_hint)
        if not chain:
            chain = [
                ModelConfig(
                    model_name=model, provider=self.config.get("provider", "openai")
                )
            ]
        return chain

    def _dispatch(self, model: str, fn_name: str, task_hint: str | None = None, **kwargs):
        """Route to model chain, respecting circuit breaker + rate limiter.

        fn_name: method name on BaseProvider (complete, complete_stream, etc.)
        Returns the result of the first successful provider call.
        """
        chain = self._resolve_chain(model, task_hint)

        last_error = None
        for model_cfg in chain:
            provider_name = model_cfg.provider.value if isinstance(model_cfg.provider, LLMProvider) else str(model_cfg.provider)
            breaker = self.rate_limiter.get_breaker(provider_name)
            if not breaker.allow_request():
                logger.warning(
                    f"Circuit open for {provider_name}, skipping {model_cfg.model_name}"
                )
                continue

            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider, model_cfg.model_name)
                # Pass cache config to Anthropic provider
                if hasattr(provider, 'cache_enabled'):
                    provider.cache_enabled = model_cfg.enable_cache
                # Some models require a fixed temperature (e.g. kimi-k2.5 only allows 1.0)
                if model_cfg.fixed_temperature is not None and "temperature" in kwargs:
                    kwargs = {**kwargs, "temperature": model_cfg.fixed_temperature}
                # Rewrite non-standard tool_call_ids for strict models (e.g. kimi-k2.5)
                if model_cfg.quirks.strict_tool_call_ids and "messages" in kwargs:
                    kwargs = {**kwargs, "messages": _rewrite_tool_call_ids(kwargs["messages"])}
                result = getattr(provider, fn_name)(model=model_cfg.model_name, **kwargs)
                breaker.record_success()
                return result, model_cfg
            except (BudgetExceededError, PermissionError):
                raise  # Non-retryable — propagate immediately
            except Exception as e:
                if _is_client_error(e):
                    # 4xx errors are our fault (bad params), not server failures.
                    # Don't poison the circuit breaker — just propagate.
                    logger.warning(f"{model_cfg.model_name} client error (not retryable): {e}")
                    raise
                breaker.record_failure()
                last_error = e
                logger.warning(f"{model_cfg.model_name} failed: {e}, trying next")
                continue

        raise last_error or ValueError(f"No available model for: {model}")

    # ── Public API ─────────────────────────────────────────────────

    def chat(
        self,
        messages: list[LLMMessage] | list[dict],
        user_id: str,
        session_id: str | None = None,
        event_id: str | None = None,
        model: str | None = None,
        temperature: float | None = None,
        metadata: dict | None = None,
        task_hint: str | None = None,
    ) -> LLMResponse:
        """Send chat request to LLM."""
        start = time.time()
        event_id = event_id or str(uuid7())
        model = self._resolve_model(model)
        temp = temperature or self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        msg_dicts = self._normalize_messages(messages)
        self._check_budget(model, msg_dicts)
        self._check_context_overflow(model, msg_dicts)

        # Enrich metadata with task_hint for cost attribution / explain.
        if task_hint:
            metadata = {**(metadata or {}), "task_hint": task_hint}

        try:
            response, model_cfg = self._dispatch(
                model,
                "complete",
                task_hint=task_hint,
                messages=msg_dicts,
                temperature=temp,
                max_tokens=max_tok,
            )
            response.latency_ms = int((time.time() - start) * 1000)
            response.cost_usd = self._active_router.calculate_cost(
                model_cfg.model_name,
                response.tokens_prompt,
                response.tokens_completion,
                cache_read_tokens=response.cache_read_tokens,
                cache_creation_tokens=response.cache_creation_tokens,
            )
            self._record_spend(response.cost_usd)
            self._log_call(
                event_id, user_id, model_cfg.provider, response, "success", metadata=metadata
            )
            if task_hint:
                self._record_auxiliary(
                    task_hint, response.tokens_prompt, response.tokens_completion,
                    response.cost_usd, response.latency_ms,
                )
            # Guard: detect degenerate responses before returning to callers.
            from core.llm.response_guard import is_degenerate
            _reason = is_degenerate(response.content, _response_guard_fps(messages))
            if _reason:
                logger.warning(
                    "Response guard (%s) on non-streaming chat: model=%s preview=%r",
                    _reason, model, (response.content or "")[:200],
                )
                response.content = ""
                response.guard_blocked = _reason
            return (
                LLMResponse(**response.model_dump())
                if isinstance(response, LLMResponse)
                else response
            )
        except Exception as e:
            self._log_call(
                event_id,
                user_id,
                self.config.get("provider", "openai"),
                None,
                "failed",
                error_message=str(e),
                latency_ms=int((time.time() - start) * 1000),
                metadata=metadata,
            )
            raise

    def chat_with_tools(
        self,
        messages: list[dict],
        tools: list[dict],
        tool_choice: str = "auto",
        model: str | None = None,
        session_id: str | None = None,
        task_hint: str | None = None,
    ) -> dict:
        """Chat with function calling."""
        model = self._resolve_model(model)
        self._check_budget(model, messages)
        self._check_context_overflow(model, messages)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        result, _ = self._dispatch(
            model,
            "complete_with_tools",
            task_hint=task_hint,
            messages=messages,
            tools=tools,
            tool_choice=tool_choice,
            temperature=temp,
            max_tokens=max_tok,
        )
        rd = dict(result) if result else {}
        # Guard non-streaming tool responses: if the LLM returned text content
        # (no tool_calls) that looks like prompt leakage, blank it out.
        _content = rd.get("content") or ""
        if _content and not rd.get("tool_calls"):
            from core.llm.response_guard import is_degenerate
            _reason = is_degenerate(_content, _response_guard_fps(messages))
            if _reason:
                logger.warning(
                    "Response guard (%s) on chat_with_tools: model=%s preview=%r",
                    _reason, model, _content[:200],
                )
                rd["content"] = ""
                rd["guard_blocked"] = _reason
        return rd

    async def chat_stream(
        self,
        messages: list[dict],
        user_id: str,
        session_id: str | None = None,
        model: str | None = None,
        task_hint: str | None = None,
    ):
        """Yield text chunks. Logs token usage at end (#5 可观测性)."""
        start = time.time()
        trace_id = str(uuid7())
        model = self._resolve_model(model)
        self._check_budget(model, messages)
        self._check_context_overflow(model, messages)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")
        _meta = {"task_hint": task_hint} if task_hint else None

        chain = self._resolve_chain(model, task_hint=task_hint)

        last_error = None
        for model_cfg in chain:
            provider_name = model_cfg.provider.value if isinstance(model_cfg.provider, LLMProvider) else str(model_cfg.provider)
            breaker = self.rate_limiter.get_breaker(provider_name)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider, model_cfg.model_name)
                usage = {"prompt": 0, "completion": 0, "cache_read": 0, "cache_creation": 0}
                _temp = model_cfg.fixed_temperature if model_cfg.fixed_temperature is not None else temp

                sync_iter = provider.complete_stream(
                    messages, model_cfg.model_name, _temp, max_tok
                )
                while True:
                    chunk = await asyncio.to_thread(next, sync_iter, _END)
                    if chunk is _END:
                        break
                    if chunk["type"] == "text":
                        yield {"type": "text", "content": chunk["content"]}
                    elif chunk["type"] == "reasoning":
                        yield {"type": "reasoning", "content": chunk["content"]}
                    elif chunk["type"] == "usage":
                        usage["prompt"] = chunk["prompt"]
                        usage["completion"] = chunk["completion"]
                        usage["cache_read"] = chunk.get("cache_read", 0)
                        usage["cache_creation"] = chunk.get("cache_creation", 0)

                breaker.record_success()
                latency = int((time.time() - start) * 1000)
                cost = self._active_router.calculate_cost(
                    model_cfg.model_name,
                    usage["prompt"],
                    usage["completion"],
                    cache_read_tokens=usage["cache_read"],
                    cache_creation_tokens=usage["cache_creation"],
                )
                self._record_spend(cost)
                self._log_call(
                    trace_id,
                    user_id,
                    model_cfg.provider,
                    LLMResponse(
                        content="[streamed]",
                        model=model_cfg.model_name,
                        provider=model_cfg.provider,
                        tokens_prompt=usage["prompt"],
                        tokens_completion=usage["completion"],
                        tokens_total=usage["prompt"] + usage["completion"],
                        latency_ms=latency,
                        cost_usd=cost,
                        cache_read_tokens=usage["cache_read"],
                        cache_creation_tokens=usage["cache_creation"],
                    ),
                    "success",
                    metadata=_meta,
                )
                yield {"type": "usage", "prompt": usage["prompt"], "completion": usage["completion"],
                       "cache_read": usage["cache_read"], "cache_creation": usage["cache_creation"]}
                return
            except (BudgetExceededError, ContextOverflowError, PermissionError):
                raise
            except Exception as e:
                if _is_client_error(e):
                    logger.warning(f"Stream {model_cfg.model_name} client error (not retryable): {e}")
                    raise
                breaker.record_failure()
                last_error = e
                logger.warning(f"Stream {model_cfg.model_name} failed: {e}")
                continue

        raise ValueError(f"All models failed for streaming: {model} (last error: {last_error})")

    async def chat_with_tools_stream(
        self,
        messages: list[dict],
        tools: list[dict],
        tool_choice: str = "auto",
        model: str | None = None,
        task_hint: str | None = None,
    ):
        """Yield tool calls and text chunks. Logs token usage at end."""
        start = time.time()
        trace_id = str(uuid7())
        model = self._resolve_model(model)
        self._check_budget(model, messages)
        self._check_context_overflow(model, messages)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")
        _meta = {"task_hint": task_hint} if task_hint else None

        chain = self._resolve_chain(model, task_hint=task_hint)

        last_error = None
        for model_cfg in chain:
            provider_name = model_cfg.provider.value if isinstance(model_cfg.provider, LLMProvider) else str(model_cfg.provider)
            breaker = self.rate_limiter.get_breaker(provider_name)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider, model_cfg.model_name)
                if hasattr(provider, 'cache_enabled'):
                    provider.cache_enabled = model_cfg.enable_cache
                usage = {"prompt": 0, "completion": 0, "cache_read": 0, "cache_creation": 0}
                _temp = model_cfg.fixed_temperature if model_cfg.fixed_temperature is not None else temp
                _messages = _rewrite_tool_call_ids(messages) if model_cfg.quirks.strict_tool_call_ids else messages
                sync_iter = provider.complete_with_tools_stream(
                    _messages, tools, model_cfg.model_name, tool_choice, _temp, max_tok
                )
                while True:
                    chunk = await asyncio.to_thread(next, sync_iter, _END)
                    if chunk is _END:
                        break
                    if chunk["type"] == "usage":
                        usage["prompt"] = chunk.get("prompt", 0)
                        usage["completion"] = chunk.get("completion", 0)
                        usage["cache_read"] = chunk.get("cache_read", 0)
                        usage["cache_creation"] = chunk.get("cache_creation", 0)
                    else:
                        yield chunk
                breaker.record_success()
                latency = int((time.time() - start) * 1000)
                cost = self._active_router.calculate_cost(
                    model_cfg.model_name,
                    usage["prompt"],
                    usage["completion"],
                    cache_read_tokens=usage["cache_read"],
                    cache_creation_tokens=usage["cache_creation"],
                )
                self._record_spend(cost)
                self._log_call(
                    trace_id, self._active_user_id, model_cfg.provider,
                    LLMResponse(
                        content="[streamed+tools]",
                        model=model_cfg.model_name, provider=model_cfg.provider,
                        tokens_prompt=usage["prompt"], tokens_completion=usage["completion"],
                        tokens_total=usage["prompt"] + usage["completion"],
                        latency_ms=latency, cost_usd=cost,
                        cache_read_tokens=usage["cache_read"],
                        cache_creation_tokens=usage["cache_creation"],
                    ),
                    "success",
                    metadata=_meta,
                )
                yield {"type": "usage", "prompt": usage["prompt"], "completion": usage["completion"],
                       "cache_read": usage["cache_read"], "cache_creation": usage["cache_creation"]}
                return
            except (BudgetExceededError, ContextOverflowError, PermissionError):
                raise
            except Exception as e:
                if _is_client_error(e):
                    logger.warning(f"Stream+tools {model_cfg.model_name} client error (not retryable): {e}")
                    raise
                breaker.record_failure()
                last_error = e
                logger.warning(f"Stream+tools {model_cfg.model_name} failed: {e}")
                continue

        raise ValueError(f"All models failed for tools streaming: {model} (last error: {last_error})")

    # ── Logging (#5 可观测性 & 回溯) ──────────────────────────────

    def _log_call(
        self,
        event_id,
        user_id,
        provider,
        response,
        status,
        error_message=None,
        latency_ms=0,
        metadata=None,
    ):
        with self._db() as db:
            log_id = str(uuid7())
            provider_str = provider.value if isinstance(provider, LLMProvider) else str(provider)
            try:
                from api.models import LLMCallLog as LLMCallLogModel
                if response:
                    db.add(LLMCallLogModel(
                        log_id=log_id, event_id=event_id, user_id=user_id,
                        provider=provider_str, model=response.model,
                        tokens_prompt=response.tokens_prompt,
                        tokens_completion=response.tokens_completion,
                        tokens_total=response.tokens_total,
                        cost_usd=response.cost_usd, latency_ms=response.latency_ms,
                        status=status,
                        call_metadata=json.dumps(metadata) if metadata else None,
                        created_at=datetime.now(timezone.utc),
                    ))
                else:
                    db.add(LLMCallLogModel(
                        log_id=log_id, event_id=event_id, user_id=user_id,
                        provider=provider_str, model="unknown",
                        tokens_prompt=0, tokens_completion=0, tokens_total=0,
                        cost_usd=0.0, latency_ms=latency_ms, status=status,
                        error_message=error_message,
                        call_metadata=json.dumps(metadata) if metadata else None,
                        created_at=datetime.now(timezone.utc),
                    ))
                db.commit()
            except Exception as e:
                db.rollback()
                logger.error(f"Failed to log LLM call: {e}")

    def get_call_logs(self, event_id=None, user_id=None) -> list[LLMCallLog]:
        with self._db() as db:
            from api.models import LLMCallLog as LLMCallLogModel
            q = db.query(LLMCallLogModel).order_by(LLMCallLogModel.created_at.desc())
            if event_id:
                q = q.filter(LLMCallLogModel.event_id == event_id)
            elif user_id:
                q = q.filter(LLMCallLogModel.user_id == user_id).limit(100)
            else:
                q = q.limit(100)
            results = q.all()
            return [
                LLMCallLog(
                    log_id=r.log_id,
                    event_id=r.event_id,
                    user_id=r.user_id,
                    provider=r.provider,
                    model=r.model,
                    tokens_prompt=r.tokens_prompt,
                    tokens_completion=r.tokens_completion,
                    tokens_total=r.tokens_total,
                    cost_usd=float(r.cost_usd),
                    latency_ms=r.latency_ms,
                    status=r.status,
                    error_message=r.error_message if hasattr(r, 'error_message') else None,
                    created_at=r.created_at,
                )
                for r in results
            ]

    @property
    def total_spend(self) -> float:
        """Current session spend in USD (#5)."""
        return self._total_spend_usd

    # ── Helpers ────────────────────────────────────────────────────

    @staticmethod
    def _normalize_messages(messages: list) -> list[dict[str, str]]:
        result: list[dict[str, str]] = []
        for m in messages:
            if isinstance(m, LLMMessage):
                d: dict[str, str] = {"role": m.role}
                if m.content is not None:
                    d["content"] = m.content
                if m.tool_calls:
                    d["tool_calls"] = m.tool_calls  # type: ignore
                if m.tool_call_id:
                    d["tool_call_id"] = m.tool_call_id
                if m.name:
                    d["name"] = m.name
                result.append(d)
            elif isinstance(m, dict):
                result.append(m)
            else:
                result.append({"role": "user", "content": str(m)})
        return result
