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
from sdk import Database

logger = logging.getLogger(__name__)


class BudgetExceededError(Exception):
    """Raised when estimated cost exceeds remaining budget."""

    pass


class LLMClient:
    """LLM client with routing, rate limiting, circuit breaker, budget control, and logging."""

    def __init__(
        self,
        db: Database | None = None,
        user_id: str | None = None,
        tenant_id: str | None = None,
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
        self.db = db or Database()
        self.user_id = user_id
        self.tenant_id = tenant_id
        self.use_default_models = use_default_models
        self.scope_context = scope_context or {}

        # Initialize scope resolver
        self.scope_resolver: ScopeResolver | None = None
        if user_id and tenant_id:
            scope_chain = ScopeChainBuilder.dev_agent(
                user_id=user_id,
                account_id=tenant_id,
                repo=self.scope_context.get("repo"),
                project=self.scope_context.get("project"),
            )
            self.scope_resolver = ScopeResolver(self.db, scope_chain)

        self._providers: dict[LLMProvider, BaseProvider] = {}
        self.router = ModelRouter(
            db=self.db, user_id=user_id, tenant_id=tenant_id, use_defaults=use_default_models
        )
        self.rate_limiter = RateLimiter()
        self.token_resolver = TokenResolver(db=self.db)
        self._load_config()
        self._init_providers()
        self._init_rate_limits()

    def set_user_context(
        self,
        user_id: str | None = None,
        tenant_id: str | None = None,
        scope_context: dict | None = None,
    ):
        """Update user context for scope-based access control.

        Args:
            scope_context: Optional scope context, e.g., {'repo': 'matrixone', 'project': 'backend'}
        """
        self.user_id = user_id
        self.tenant_id = tenant_id
        self.scope_context = scope_context or {}

        # Rebuild scope resolver
        if user_id and tenant_id and scope_context:
            scope_chain = ScopeChainBuilder.dev_agent(
                user_id=user_id,
                account_id=tenant_id,
                repo=scope_context.get("repo"),
                project=scope_context.get("project"),
            )
            self.scope_resolver = ScopeResolver(self.db, scope_chain)

        # Reload router with new context
        self.router = ModelRouter(
            db=self.db, user_id=user_id, tenant_id=tenant_id, use_defaults=self.use_default_models
        )
        # Re-initialize providers with new API keys
        self._init_providers()

    # ── Config (#4 动态配置) ───────────────────────────────────────

    def _load_config(self) -> None:
        """Load config: DB → env → defaults."""
        config = None
        try:
            row = self.db.fetchone(
                "SELECT value FROM configs WHERE key_name = 'llm_config' LIMIT 1"
            )
            if row:
                config = json.loads(row["value"])
        except Exception:
            pass
        if not config:
            config = {
                "provider": os.getenv("LLM_PROVIDER", "openai"),
                "model": os.getenv("LLM_MODEL", "gpt-4o"),
                "temperature": float(os.getenv("LLM_TEMPERATURE", "0.7")),
                "max_tokens": int(os.getenv("LLM_MAX_TOKENS", "2000")),
                "budget_usd": float(os.getenv("LLM_BUDGET_USD", "0")),  # 0 = unlimited
            }
        self.config = config
        self._total_spend_usd = 0.0

    def reload_config(self):
        """Hot reload config + model registry (#4)."""
        self._load_config()
        self.router.reload(self.db)
        self._init_rate_limits()
        logger.info("LLM config reloaded")

    # ── Provider init (#3 异构) ────────────────────────────────────

    def _init_providers(self) -> None:
        """Initialize provider clients once (connection pooling)."""
        for provider_name, cls, extra in [
            (
                "openai",
                OpenAIProvider,
                lambda k: {
                    "base_url": self.config.get("openai_base_url") or os.getenv("OPENAI_BASE_URL")
                },
            ),
            ("groq", GroqProvider, lambda k: {}),
            ("anthropic", AnthropicProvider, lambda k: {}),
        ]:
            api_key = self._get_api_key(provider_name)
            if api_key:
                try:
                    kwargs = extra(api_key)
                    kwargs = {k: v for k, v in kwargs.items() if v}
                    self._providers[LLMProvider(provider_name)] = cls(api_key, **kwargs)
                    logger.debug(f"Initialized {provider_name} provider")
                except Exception as e:
                    logger.warning(f"Failed to initialize {provider_name} provider: {e}")
            else:
                logger.debug(f"No API key found for {provider_name}, skipping")

    def _get_api_key(self, provider: str) -> str | None:
        """Get API key with scope-based resolution.

        Priority: scope_resolver > user > tenant > configs table
        """
        # 1. Try ScopeResolver (supports extended scopes like repo/project)
        if self.scope_resolver:
            token = self.scope_resolver.resolve_token("llm", provider)
            if token:
                return token.get("encrypted_value") or token.get("secret_ref")

        # 2. Fallback to TokenResolver (user/tenant scope only)
        if self.user_id or self.tenant_id:
            token = self._resolve_llm_token(provider)
            if token:
                val = token.encrypted_value or token.secret_ref
                return str(val) if val else None

        # 3. Fallback to configs table (global)
        try:
            row = self.db.fetchone(
                "SELECT value FROM configs WHERE key_name = %s AND scope_type = 'global' LIMIT 1",
                (f"{provider}_api_key",),
            )
            if row:
                return str(row.get("value", "")) or None
        except Exception:
            pass

        return None

    def _resolve_llm_token(self, provider: str):
        """Resolve LLM token using TokenResolver logic."""
        # 1. User-scoped token
        if self.user_id:
            query = """
                SELECT * FROM tokens
                WHERE type = 'llm' AND provider = %s
                AND scope_user_id = %s AND is_active = TRUE
                ORDER BY created_at DESC LIMIT 1
            """
            result = self.db.fetchone(query, (provider, self.user_id))
            if result:
                return self._token_from_row(result)

        # 2. Tenant-scoped token
        if self.tenant_id:
            query = """
                SELECT * FROM tokens
                WHERE type = 'llm' AND provider = %s
                AND scope_tenant_id = %s AND is_active = TRUE
                ORDER BY created_at DESC LIMIT 1
            """
            result = self.db.fetchone(query, (provider, self.tenant_id))
            if result:
                return self._token_from_row(result)

        return None

    def _token_from_row(self, row):
        """Convert DB row to simple token object."""
        from types import SimpleNamespace

        return SimpleNamespace(
            token_id=row["token_id"],
            provider=row["provider"],
            encrypted_value=row.get("encrypted_value"),
            secret_ref=row.get("secret_ref"),
        )

    def _init_rate_limits(self) -> None:
        for m in self.router.list_models():
            self.rate_limiter.configure(m.model_name, m.rpm_limit, m.tpm_limit)

    def _get_provider(self, p: LLMProvider) -> BaseProvider:
        provider = self._providers.get(p)
        if not provider:
            scope_info = []
            if self.user_id:
                scope_info.append(f"user '{self.user_id}'")
            if self.tenant_id:
                scope_info.append(f"tenant '{self.tenant_id}'")
            scope_str = " for " + " and ".join(scope_info) if scope_info else ""

            raise ValueError(
                f"Provider '{p.value}' is not configured{scope_str}.\n"
                f"Please set the API key using one of:\n"
                f"  1. Database: INSERT INTO tokens (type, provider, scope_user_id, encrypted_value) VALUES ('llm', '{p.value}', '{self.user_id or 'USER_ID'}', 'YOUR_KEY')\n"
                f"  2. Environment: export {p.value.upper()}_API_KEY=YOUR_KEY\n"
                f"  3. Config: Add '{p.value}_api_key' to configs table"
            )
        return provider

    def _resolve_model(self, model: str | None) -> str:
        return model or self.config.get("model", "gpt-4o")

    # ── Budget control (#7) ────────────────────────────────────────

    def _check_budget(self, model: str, estimated_tokens: int = 1000):
        budget = self.config.get("budget_usd", 0)
        if budget <= 0:
            return  # unlimited
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
            if self.tenant_id:
                scope_info.append(f"tenant '{self.tenant_id}'")

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

    def _dispatch(self, model: str, fn_name: str, task_hint: str | None = None, **kwargs):
        """Route to model chain, respecting circuit breaker + rate limiter.

        fn_name: method name on BaseProvider (complete, complete_stream, etc.)
        Returns the result of the first successful provider call.
        """
        # Check permission before routing
        self._check_model_permission(model)

        chain = self.router.route(model, task_hint=task_hint)
        if not chain:
            # Unknown model — try direct with default provider
            chain = [
                ModelConfig(
                    model_name=model, provider=LLMProvider(self.config.get("provider", "openai"))
                )
            ]

        last_error = None
        for model_cfg in chain:
            breaker = self.rate_limiter.get_breaker(model_cfg.provider.value)
            if not breaker.allow_request():
                logger.warning(
                    f"Circuit open for {model_cfg.provider.value}, skipping {model_cfg.model_name}"
                )
                continue

            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider)
                result = getattr(provider, fn_name)(model=model_cfg.model_name, **kwargs)
                breaker.record_success()
                return result, model_cfg
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

        self._check_budget(model)
        msg_dicts = self._normalize_messages(messages)

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
                model_cfg.model_name, response.tokens_prompt, response.tokens_completion
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
                LLMProvider(self.config.get("provider", "openai")),
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
        self._check_budget(model)
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
        self._check_budget(model)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        chain = self.router.route(model, task_hint=task_hint)
        if not chain:
            chain = [
                ModelConfig(
                    model_name=model, provider=LLMProvider(self.config.get("provider", "openai"))
                )
            ]

        for model_cfg in chain:
            breaker = self.rate_limiter.get_breaker(model_cfg.provider.value)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider)
                usage = {"prompt": 0, "completion": 0}

                for chunk in provider.complete_stream(
                    messages, model_cfg.model_name, temp, max_tok
                ):
                    if chunk["type"] == "text":
                        yield chunk["content"]
                    elif chunk["type"] == "usage":
                        usage["prompt"] = chunk["prompt"]
                        usage["completion"] = chunk["completion"]

                breaker.record_success()
                latency = int((time.time() - start) * 1000)
                cost = self.router.calculate_cost(
                    model_cfg.model_name, usage["prompt"], usage["completion"]
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
                    ),
                    "success",
                )
                return
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
        self._check_budget(model)
        temp = self.config.get("temperature", 0.7)
        max_tok = self.config.get("max_tokens")

        chain = self.router.route(model, task_hint=task_hint)
        if not chain:
            chain = [
                ModelConfig(
                    model_name=model, provider=LLMProvider(self.config.get("provider", "openai"))
                )
            ]

        for model_cfg in chain:
            breaker = self.rate_limiter.get_breaker(model_cfg.provider.value)
            if not breaker.allow_request():
                continue
            try:
                self.rate_limiter.wait_and_acquire(model_cfg.model_name, estimated_tokens=500)
                provider = self._get_provider(model_cfg.provider)
                for chunk in provider.complete_with_tools_stream(
                    messages, tools, model_cfg.model_name, tool_choice, temp, max_tok
                ):
                    if chunk["type"] != "usage":
                        yield chunk
                breaker.record_success()
                return
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
        try:
            if response:
                self.db.execute(
                    """INSERT INTO llm_call_logs (
                        log_id, event_id, user_id, provider, model,
                        tokens_prompt, tokens_completion, tokens_total,
                        cost_usd, latency_ms, status, metadata, created_at
                    ) VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s)""",
                    (
                        log_id,
                        event_id,
                        user_id,
                        provider.value,
                        response.model,
                        response.tokens_prompt,
                        response.tokens_completion,
                        response.tokens_total,
                        response.cost_usd,
                        response.latency_ms,
                        status,
                        json.dumps(metadata) if metadata else None,
                        datetime.now(timezone.utc),
                    ),
                )
            else:
                self.db.execute(
                    """INSERT INTO llm_call_logs (
                        log_id, event_id, user_id, provider, model,
                        tokens_prompt, tokens_completion, tokens_total,
                        cost_usd, latency_ms, status, error_message, metadata, created_at
                    ) VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s)""",
                    (
                        log_id,
                        event_id,
                        user_id,
                        provider.value,
                        "unknown",
                        0,
                        0,
                        0,
                        0.0,
                        latency_ms,
                        status,
                        error_message,
                        json.dumps(metadata) if metadata else None,
                        datetime.now(timezone.utc),
                    ),
                )
        except Exception as e:
            logger.error(f"Failed to log LLM call: {e}")

    def get_call_logs(self, event_id=None, user_id=None) -> list[LLMCallLog]:
        if event_id:
            results = self.db.fetchall(
                "SELECT * FROM llm_call_logs WHERE event_id = %s ORDER BY created_at DESC",
                (event_id,),
            )
        elif user_id:
            results = self.db.fetchall(
                "SELECT * FROM llm_call_logs WHERE user_id = %s ORDER BY created_at DESC LIMIT 100",
                (user_id,),
            )
        else:
            results = self.db.fetchall(
                "SELECT * FROM llm_call_logs ORDER BY created_at DESC LIMIT 100"
            )
        return [
            LLMCallLog(
                log_id=r["log_id"],
                event_id=r["event_id"],
                user_id=r["user_id"],
                provider=LLMProvider(r["provider"]),
                model=r["model"],
                tokens_prompt=r["tokens_prompt"],
                tokens_completion=r["tokens_completion"],
                tokens_total=r["tokens_total"],
                cost_usd=float(r["cost_usd"]),
                latency_ms=r["latency_ms"],
                status=r["status"],
                error_message=r.get("error_message"),
                created_at=r["created_at"],
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
