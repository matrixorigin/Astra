"""LLM client with provider abstraction, routing, rate limiting, circuit breaker, and call logging."""

import json
import logging
import os
import time
from datetime import datetime, timezone

from uuid_utils import uuid7

from core.llm.models import LLMCallLog, LLMMessage, LLMProvider, LLMResponse
from core.llm.providers import AnthropicProvider, BaseProvider, GroqProvider, OpenAIProvider
from core.llm.rate_limiter import RateLimiter
from core.llm.router import ModelConfig, ModelRouter
from core.repos.token_resolver import TokenResolver
from core.scope.scope_resolver import ScopeChainBuilder, ScopeResolver
from sqlalchemy import text
from sqlalchemy.orm import Session
from api.database import get_db_session

logger = logging.getLogger(__name__)


class BudgetExceededError(Exception):
    """Raised when estimated cost exceeds remaining budget."""

    pass


_PROVIDER_DEFAULT_MODELS = {
    "openai": "gpt-4o",
    "deepseek": "deepseek-chat",
    "anthropic": "claude-3-5-sonnet-20241022",
    "groq": "llama3-70b",
}


def _default_model_for_provider(provider: str) -> str:
    return _PROVIDER_DEFAULT_MODELS.get(provider, "gpt-4o")


class LLMClient:
    """LLM client with routing, rate limiting, circuit breaker, budget control, and logging."""

    def __init__(
        self,
        db: Session | None = None,
        user_id: str | None = None,
        use_default_models: bool = True,
        scope_context: dict | None = None,
    ) -> None:
        """Initialize LLM client.

        Args:
            use_default_models: If True, use DEFAULT_MODELS as fallback. Set to False
                               in production to enforce strict scope-based access control.
            scope_context: Optional scope context for resolver, e.g.,
                          {'repo': 'matrixone', 'project': 'backend'}
        """
        self.db = db or next(get_db_session())
        self.user_id = user_id
        self.use_default_models = use_default_models
        self.scope_context = scope_context or {}

        # Initialize scope resolver
        self.scope_resolver: ScopeResolver | None = None
        if user_id:
            scope_chain = ScopeChainBuilder.dev_agent(
                user_id=user_id,
                repo=self.scope_context.get("repo"),
                project=self.scope_context.get("project"),
            )
            self.scope_resolver = ScopeResolver(self.db, scope_chain)

        self._providers: dict[str, BaseProvider] = {}
        self.router = ModelRouter(
            db=self.db, user_id=user_id, use_defaults=use_default_models
        )
        self.rate_limiter = RateLimiter()
        self.token_resolver = TokenResolver(db=self.db)
        self._load_config()
        self._init_providers()
        self._init_rate_limits()

    def set_user_context(
        self,
        user_id: str | None = None,
        scope_context: dict | None = None,
    ):
        """Update user context for scope-based access control.

        Args:
            scope_context: Optional scope context, e.g., {'repo': 'matrixone', 'project': 'backend'}
        """
        self.user_id = user_id
        self.scope_context = scope_context or {}

        # Rebuild scope resolver
        if user_id:
            scope_chain = ScopeChainBuilder.dev_agent(
                user_id=user_id,
                repo=(scope_context or {}).get("repo"),
                project=(scope_context or {}).get("project"),
            )
            self.scope_resolver = ScopeResolver(self.db, scope_chain)

        # Reload router with new context
        self.router = ModelRouter(
            db=self.db, user_id=user_id, use_defaults=self.use_default_models
        )
        # Re-initialize providers with new API keys
        self._init_providers()

    # ── Config (#4 动态配置) ───────────────────────────────────────

    def _load_config(self) -> None:
        """Load config: DB → env → auto-detect from registered tokens."""
        config = None
        try:
            result = self.db.execute(
                text("SELECT value FROM configs WHERE key_name = 'llm_config' LIMIT 1")
            )
            row = result.first()
            if row:
                config = json.loads(row.value)
        except Exception:
            pass
        if not config:
            # Auto-detect default provider/model from first active token
            provider = os.getenv("LLM_PROVIDER", "")
            model = os.getenv("LLM_MODEL", "")
            if not provider:
                try:
                    row = self.db.execute(
                        text("SELECT provider FROM tokens WHERE type='llm' AND is_active=TRUE ORDER BY created_at DESC LIMIT 1")
                    ).first()
                    if row:
                        provider = row[0]
                except Exception:
                    pass
            provider = provider or "openai"
            if not model:
                model = _default_model_for_provider(provider)
            config = {
                "provider": provider,
                "model": model,
                "temperature": float(os.getenv("LLM_TEMPERATURE", "0.7")),
                "max_tokens": int(os.getenv("LLM_MAX_TOKENS", "2000")),
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
        self.router.reload(self.db)
        self._init_rate_limits()
        logger.info("LLM config reloaded")

    # ── Provider init (#3 异构) ────────────────────────────────────

    def _init_providers(self) -> None:
        """Initialize provider clients once (connection pooling).

        All providers use OpenAI-compatible protocol. base_url from token metadata.
        """
        # Discover all active LLM tokens from DB
        providers_to_init = set()
        try:
            rows = self.db.execute(
                text("SELECT DISTINCT provider FROM tokens WHERE type='llm' AND is_active=TRUE")
            ).fetchall()
            for row in rows:
                providers_to_init.add(row[0])
        except Exception:
            pass

        # Always try well-known providers
        for p in ["openai", "groq", "anthropic"]:
            providers_to_init.add(p)

        for provider_name in providers_to_init:
            api_key = self._get_api_key(provider_name)
            if not api_key:
                logger.debug(f"No API key found for {provider_name}, skipping")
                continue
            try:
                base_url = self._get_provider_base_url(provider_name)
                # Groq and Anthropic have their own clients
                if provider_name == "groq" and not base_url:
                    self._providers[provider_name] = GroqProvider(api_key)
                elif provider_name == "anthropic" and not base_url:
                    self._providers[provider_name] = AnthropicProvider(api_key)
                else:
                    # Everything else: OpenAI-compatible with optional base_url
                    kwargs = {"base_url": base_url} if base_url else {}
                    self._providers[provider_name] = OpenAIProvider(api_key, **kwargs)
                logger.debug(f"Initialized {provider_name} provider" + (f" (base_url={base_url})" if base_url else ""))
            except Exception as e:
                logger.warning(f"Failed to initialize {provider_name} provider: {e}")

    def _get_provider_base_url(self, provider: str) -> str | None:
        """Get base_url from token metadata or config."""
        try:
            row = self.db.execute(
                text("SELECT metadata FROM tokens WHERE type='llm' AND provider=:provider AND is_active=TRUE ORDER BY created_at DESC LIMIT 1"),
                {"provider": provider},
            ).first()
            if row and row.metadata:
                meta = json.loads(row.metadata) if isinstance(row.metadata, str) else row.metadata
                if meta.get("base_url"):
                    return meta["base_url"]
        except Exception:
            pass
        return self.config.get(f"{provider}_base_url")

    def _get_api_key(self, provider: str) -> str | None:
        """Get API key with scope-based resolution.

        Priority: scope_resolver > user > configs table
        """
        # 1. Try ScopeResolver (supports extended scopes like repo/project)
        if self.scope_resolver:
            token = self.scope_resolver.resolve_token("llm", provider)
            if token:
                return token.get("encrypted_value") or token.get("secret_ref")

        # 2. Fallback to TokenResolver (user → global)
        token = self._resolve_llm_token(provider)
        if token:
            val = token.encrypted_value or token.secret_ref
            return str(val) if val else None

        # 3. Fallback to configs table (global)
        try:
            result = self.db.execute(
                text("SELECT value FROM configs WHERE key_name = :key_name AND scope_type = 'global' LIMIT 1"),
                {"key_name": f"{provider}_api_key"}
            )
            row = result.first()
            if row:
                return str(row.value) or None
        except Exception:
            pass

        return None

    def _resolve_llm_token(self, provider: str):
        """Resolve LLM token: user-scoped first, then global."""
        queries = []
        if self.user_id:
            queries.append((
                "SELECT * FROM tokens WHERE type='llm' AND provider=:provider AND scope_user_id=:user_id AND is_active=TRUE ORDER BY created_at DESC LIMIT 1",
                {"provider": provider, "user_id": self.user_id},
            ))
        queries.append((
            "SELECT * FROM tokens WHERE type='llm' AND provider=:provider AND scope_user_id IS NULL AND is_active=TRUE ORDER BY created_at DESC LIMIT 1",
            {"provider": provider},
        ))
        for sql, params in queries:
            result = self.db.execute(text(sql), params)
            row = result.first()
            if row:
                return self._token_from_row(row)
        return None

    def _token_from_row(self, row):
        """Convert DB row to simple token object."""
        from types import SimpleNamespace

        return SimpleNamespace(
            token_id=row.token_id,
            provider=row.provider,
            encrypted_value=row.encrypted_value if hasattr(row, 'encrypted_value') else None,
            secret_ref=row.secret_ref if hasattr(row, 'secret_ref') else None,
        )

    def _init_rate_limits(self) -> None:
        for m in self.router.list_models():
            self.rate_limiter.configure(m.model_name, m.rpm_limit, m.tpm_limit)

    def _get_provider(self, p) -> BaseProvider:
        name = p.value if isinstance(p, LLMProvider) else str(p)
        provider = self._providers.get(name)
        if not provider:
            raise ValueError(
                f"Provider '{name}' is not configured.\n"
                f"Register via: mo-admin token create --type llm --provider {name} --scope global"
            )
        return provider

    def _resolve_model(self, model: str | None) -> str:
        return model or self.config.get("model", "gpt-4o")

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
        estimated_cost = self.router.estimate_cost(model, estimated_tokens)
        if self._total_spend_usd + estimated_cost > budget:
            raise BudgetExceededError(
                f"Estimated cost ${estimated_cost:.4f} would exceed budget "
                f"(spent ${self._total_spend_usd:.4f} of ${budget:.2f})"
            )

    def _record_spend(self, cost: float):
        self._total_spend_usd += cost

    # ── Core dispatch with circuit breaker (#6) ────────────────────

    def _check_model_permission(self, model: str):
        """Check if current user has permission to use the model."""
        available_models = self.router.list_models()
        model_names = [m.model_name for m in available_models]

        if model not in model_names:
            # Build detailed error message
            scope_info = []
            if self.user_id:
                scope_info.append(f"user '{self.user_id}'")

            scope_str = " and ".join(scope_info) if scope_info else "current scope"

            error_msg = [
                f"Model '{model}' is not available for {scope_str}.",
                f"\nAvailable models ({len(model_names)}):",
            ]

            # Group by provider for better readability
            by_provider: dict[str, list[str]] = {}
            for m in available_models:
                provider = m.provider.value
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
        chain = self.router.route(model, task_hint=task_hint)
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
                provider = self._get_provider(model_cfg.provider)
                # Pass cache config to Anthropic provider
                if hasattr(provider, 'cache_enabled'):
                    provider.cache_enabled = model_cfg.enable_cache
                result = getattr(provider, fn_name)(model=model_cfg.model_name, **kwargs)
                breaker.record_success()
                return result, model_cfg
            except (BudgetExceededError, PermissionError):
                raise  # Non-retryable — propagate immediately
            except Exception as e:
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
            response.cost_usd = self.router.calculate_cost(
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
        return dict(result) if result else {}

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
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        chain = self._resolve_chain(model, task_hint=task_hint)

        for model_cfg in chain:
            breaker = self.rate_limiter.get_breaker(model_cfg.provider.value)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider)
                usage = {"prompt": 0, "completion": 0, "cache_read": 0, "cache_creation": 0}

                for chunk in provider.complete_stream(
                    messages, model_cfg.model_name, temp, max_tok
                ):
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
                cost = self.router.calculate_cost(
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
                )
                return
            except (BudgetExceededError, PermissionError):
                raise
            except Exception as e:
                breaker.record_failure()
                logger.warning(f"Stream {model_cfg.model_name} failed: {e}")
                continue

        raise ValueError(f"All models failed for streaming: {model}")

    async def chat_with_tools_stream(
        self,
        messages: list[dict],
        tools: list[dict],
        tool_choice: str = "auto",
        model: str | None = None,
        task_hint: str | None = None,
    ):
        """Yield tool calls and text chunks."""
        model = self._resolve_model(model)
        self._check_budget(model, messages)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        chain = self._resolve_chain(model, task_hint=task_hint)

        for model_cfg in chain:
            breaker = self.rate_limiter.get_breaker(model_cfg.provider.value)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider)
                if hasattr(provider, 'cache_enabled'):
                    provider.cache_enabled = model_cfg.enable_cache
                for chunk in provider.complete_with_tools_stream(
                    messages, tools, model_cfg.model_name, tool_choice, temp, max_tok
                ):
                    if chunk["type"] != "usage":
                        yield chunk
                breaker.record_success()
                return
            except (BudgetExceededError, PermissionError):
                raise
            except Exception as e:
                breaker.record_failure()
                logger.warning(f"Stream+tools {model_cfg.model_name} failed: {e}")
                continue

        raise ValueError(f"All models failed for tools streaming: {model}")

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
        log_id = str(uuid7())
        provider_str = provider.value if isinstance(provider, LLMProvider) else str(provider)
        try:
            if response:
                self.db.execute(
                    text("""INSERT INTO llm_call_logs (
                        log_id, event_id, user_id, provider, model,
                        tokens_prompt, tokens_completion, tokens_total,
                        cost_usd, latency_ms, status, metadata, created_at
                    ) VALUES (:log_id, :event_id, :user_id, :provider, :model,
                        :tp, :tc, :tt, :cost, :lat, :status, :meta, :ts)"""),
                    {
                        "log_id": log_id, "event_id": event_id, "user_id": user_id,
                        "provider": provider_str, "model": response.model,
                        "tp": response.tokens_prompt, "tc": response.tokens_completion,
                        "tt": response.tokens_total, "cost": response.cost_usd,
                        "lat": response.latency_ms, "status": status,
                        "meta": json.dumps(metadata) if metadata else None,
                        "ts": datetime.now(timezone.utc),
                    },
                )
            else:
                self.db.execute(
                    text("""INSERT INTO llm_call_logs (
                        log_id, event_id, user_id, provider, model,
                        tokens_prompt, tokens_completion, tokens_total,
                        cost_usd, latency_ms, status, error_message, metadata, created_at
                    ) VALUES (:log_id, :event_id, :user_id, :provider, :model,
                        0, 0, 0, 0.0, :lat, :status, :err, :meta, :ts)"""),
                    {
                        "log_id": log_id, "event_id": event_id, "user_id": user_id,
                        "provider": provider_str, "model": "unknown",
                        "lat": latency_ms, "status": status, "err": error_message,
                        "meta": json.dumps(metadata) if metadata else None,
                        "ts": datetime.now(timezone.utc),
                    },
                )
            self.db.commit()
        except Exception as e:
            self.db.rollback()
            logger.error(f"Failed to log LLM call: {e}")

    def get_call_logs(self, event_id=None, user_id=None) -> list[LLMCallLog]:
        if event_id:
            result = self.db.execute(
                text("SELECT * FROM llm_call_logs WHERE event_id = :event_id ORDER BY created_at DESC"),
                {"event_id": event_id}
            )
            results = result.fetchall()
        elif user_id:
            result = self.db.execute(
                text("SELECT * FROM llm_call_logs WHERE user_id = :user_id ORDER BY created_at DESC LIMIT 100"),
                {"user_id": user_id}
            )
            results = result.fetchall()
        else:
            result = self.db.execute(
                text("SELECT * FROM llm_call_logs ORDER BY created_at DESC LIMIT 100")
            )
            results = result.fetchall()
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
