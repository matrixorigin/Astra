"""LLM provider adapters with connection pooling and retry."""

import logging
import random
import time
from abc import ABC, abstractmethod
from collections.abc import Iterator
from typing import Any

from core.llm.models import LLMProvider, LLMResponse

logger = logging.getLogger(__name__)

MAX_RETRIES = 3
RETRY_BASE_DELAY = 1.0


def _should_retry(error: Exception) -> bool:
    err_name = type(error).__name__
    retryable = {
        "RateLimitError",
        "APITimeoutError",
        "InternalServerError",
        "APIConnectionError",
        "OverloadedError",
    }
    if any(r in err_name for r in retryable):
        return True
    return any(str(code) in str(error) for code in (429, 500, 502, 503, 504))


class BaseProvider(ABC):
    """Base class for LLM provider adapters."""

    provider: LLMProvider

    @abstractmethod
    def complete(
        self, messages: list[dict], model: str, temperature: float, max_tokens: int | None
    ) -> LLMResponse: ...

    @abstractmethod
    def complete_stream(
        self, messages: list[dict], model: str, temperature: float, max_tokens: int | None
    ) -> Iterator[dict]: ...

    @abstractmethod
    def complete_with_tools(
        self,
        messages: list[dict],
        tools: list[dict],
        model: str,
        tool_choice: str,
        temperature: float,
        max_tokens: int | None,
    ) -> dict: ...

    @abstractmethod
    def complete_with_tools_stream(
        self,
        messages: list[dict],
        tools: list[dict],
        model: str,
        tool_choice: str,
        temperature: float,
        max_tokens: int | None,
    ) -> Iterator[dict]: ...

    def _with_retry(self, fn, *args, **kwargs):
        last_error = None
        for attempt in range(MAX_RETRIES):
            try:
                return fn(*args, **kwargs)
            except Exception as e:
                last_error = e
                if attempt < MAX_RETRIES - 1 and _should_retry(e):
                    delay = RETRY_BASE_DELAY * (2**attempt)
                    jitter = random.uniform(0, delay * 0.5)
                    time.sleep(delay + jitter)
                    logger.warning(f"Retry {attempt + 1}/{MAX_RETRIES}: {e}")
                else:
                    raise
        raise last_error


def _extract_openai_cached_tokens(usage) -> int:
    """Extract cached tokens from OpenAI usage (automatic prefix caching)."""
    details = getattr(usage, "prompt_tokens_details", None)
    if details:
        return getattr(details, "cached_tokens", 0) or 0
    return 0


def _accumulate_tool_calls(response_iter) -> Iterator[dict]:
    """Shared tool call accumulation for OpenAI-compatible streaming."""
    buf: dict[int, dict] = {}
    truncated = False
    for chunk in response_iter:
        if chunk.usage:
            yield {
                "type": "usage",
                "prompt": chunk.usage.prompt_tokens,
                "completion": chunk.usage.completion_tokens,
                "cache_read": _extract_openai_cached_tokens(chunk.usage),
            }
            continue
        if not chunk.choices:
            continue
        choice = chunk.choices[0]
        # Detect output truncation (max_tokens reached).
        if choice.finish_reason == "length":
            truncated = True
        delta = choice.delta
        # OpenAI o-series reasoning tokens
        reasoning = getattr(delta, "reasoning_content", None)
        if reasoning:
            yield {"type": "reasoning", "content": reasoning}
        if delta.content:
            yield {"type": "text", "content": delta.content}
        if delta.tool_calls:
            for tc in delta.tool_calls:
                idx = tc.index
                if idx not in buf:
                    buf[idx] = {
                        "id": tc.id or f"idx_{idx}",
                        "type": tc.type or "function",
                        "function": {"name": "", "arguments": ""},
                    }
                elif tc.id:
                    buf[idx]["id"] = tc.id
                if tc.function and tc.function.name:
                    prev = buf[idx]["function"]["name"]
                    buf[idx]["function"]["name"] = tc.function.name
                    if not prev:
                        # First time we see the tool name — notify caller
                        yield {"type": "tool_call_start", "name": tc.function.name}
                if tc.function and tc.function.arguments:
                    buf[idx]["function"]["arguments"] += tc.function.arguments
    for tc in buf.values():
        if tc["function"]["name"]:
            if truncated:
                logger.warning("tool_call %s truncated by max_tokens, arguments incomplete",
                               tc["function"]["name"])
                tc["_truncated"] = True
            elif not tc["function"]["arguments"]:
                logger.warning("LLM emitted tool_call %s with empty arguments",
                               tc["function"]["name"])
            yield {"type": "tool_call", "data": tc}


def _extract_tool_calls(msg) -> list[dict]:
    if not msg.tool_calls:
        return []
    return [
        {
            "id": tc.id,
            "type": tc.type,
            "function": {"name": tc.function.name, "arguments": tc.function.arguments},
        }
        for tc in msg.tool_calls
    ]


def _extract_usage(resp) -> dict:
    return {
        "prompt_tokens": resp.usage.prompt_tokens,
        "completion_tokens": resp.usage.completion_tokens,
        "total_tokens": resp.usage.total_tokens,
        "cache_read_tokens": _extract_openai_cached_tokens(resp.usage),
    }


class OpenAIProvider(BaseProvider):
    """OpenAI provider with connection pooling."""

    provider = LLMProvider.OPENAI

    def __init__(self, api_key: str, base_url: str | None = None):
        import openai
        import httpx

        kwargs: dict[str, Any] = {
            "api_key": api_key,
            # Connect fast, allow generous read for streaming (120s idle per chunk).
            "timeout": httpx.Timeout(connect=10.0, read=120.0, write=30.0, pool=10.0),
        }
        if base_url:
            kwargs["base_url"] = base_url
        self._client = openai.OpenAI(**kwargs)

    def complete(self, messages, model, temperature, max_tokens) -> LLMResponse:
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model, messages=messages, temperature=temperature, max_tokens=max_tokens
            )
        )
        cache_read = _extract_openai_cached_tokens(resp.usage)
        return LLMResponse(
            content=resp.choices[0].message.content or "",
            model=resp.model,
            provider=self.provider,
            tokens_prompt=resp.usage.prompt_tokens,
            tokens_completion=resp.usage.completion_tokens,
            tokens_total=resp.usage.total_tokens,
            latency_ms=0,
            cost_usd=0.0,
            cache_read_tokens=cache_read,
        )

    def complete_stream(self, messages, model, temperature, max_tokens):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                temperature=temperature,
                max_tokens=max_tokens,
                stream=True,
                stream_options={"include_usage": True},
            )
        )
        for chunk in resp:
            if chunk.usage:
                yield {
                    "type": "usage",
                    "prompt": chunk.usage.prompt_tokens,
                    "completion": chunk.usage.completion_tokens,
                    "cache_read": _extract_openai_cached_tokens(chunk.usage),
                }
            elif chunk.choices and chunk.choices[0].delta:
                delta = chunk.choices[0].delta
                # OpenAI o-series reasoning tokens
                reasoning = getattr(delta, "reasoning_content", None)
                if reasoning:
                    yield {"type": "reasoning", "content": reasoning}
                if delta.content:
                    yield {"type": "text", "content": delta.content}

    def complete_with_tools(self, messages, tools, model, tool_choice, temperature, max_tokens):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                tools=tools,
                tool_choice=tool_choice,
                temperature=temperature,
                max_tokens=max_tokens,
            )
        )
        return {
            "content": resp.choices[0].message.content or "",
            "tool_calls": _extract_tool_calls(resp.choices[0].message),
            "usage": _extract_usage(resp),
        }

    def complete_with_tools_stream(
        self, messages, tools, model, tool_choice, temperature, max_tokens
    ):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                tools=tools,
                tool_choice=tool_choice,
                temperature=temperature,
                max_tokens=max_tokens,
                stream=True,
                stream_options={"include_usage": True},
            )
        )
        yield from _accumulate_tool_calls(resp)


class GroqProvider(BaseProvider):
    """Groq provider — OpenAI-compatible API."""

    provider = LLMProvider.GROQ

    def __init__(self, api_key: str):
        from groq import Groq
        import httpx

        self._client = Groq(
            api_key=api_key,
            timeout=httpx.Timeout(connect=10.0, read=120.0, write=30.0, pool=10.0),
        )

    def complete(self, messages, model, temperature, max_tokens) -> LLMResponse:
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model, messages=messages, temperature=temperature, max_tokens=max_tokens
            )
        )
        return LLMResponse(
            content=resp.choices[0].message.content or "",
            model=resp.model,
            provider=self.provider,
            tokens_prompt=resp.usage.prompt_tokens,
            tokens_completion=resp.usage.completion_tokens,
            tokens_total=resp.usage.total_tokens,
            latency_ms=0,
            cost_usd=0.0,
        )

    def complete_stream(self, messages, model, temperature, max_tokens):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                temperature=temperature,
                max_tokens=max_tokens,
                stream=True,
            )
        )
        for chunk in resp:
            if chunk.choices and chunk.choices[0].delta.content:
                yield {"type": "text", "content": chunk.choices[0].delta.content}

    def complete_with_tools(self, messages, tools, model, tool_choice, temperature, max_tokens):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                tools=tools,
                tool_choice=tool_choice,
                temperature=temperature,
                max_tokens=max_tokens,
            )
        )
        return {
            "content": resp.choices[0].message.content or "",
            "tool_calls": _extract_tool_calls(resp.choices[0].message),
            "usage": _extract_usage(resp),
        }

    def complete_with_tools_stream(
        self, messages, tools, model, tool_choice, temperature, max_tokens
    ):
        resp = self._with_retry(
            lambda: self._client.chat.completions.create(
                model=model,
                messages=messages,
                tools=tools,
                tool_choice=tool_choice,
                temperature=temperature,
                max_tokens=max_tokens,
                stream=True,
            )
        )
        yield from _accumulate_tool_calls(resp)


class AnthropicProvider(BaseProvider):
    """Anthropic provider with prompt caching support.

    Automatically marks system prompt and tool definitions as cacheable
    using Anthropic's cache_control: {"type": "ephemeral"}.
    Cache hits reduce input token cost by ~90%.
    """

    provider = LLMProvider.ANTHROPIC

    def __init__(self, api_key: str):
        import anthropic
        import httpx

        self._client = anthropic.Anthropic(
            api_key=api_key,
            timeout=httpx.Timeout(connect=10.0, read=120.0, write=30.0, pool=10.0),
        )
        self.cache_enabled = True  # Set by LLMClient._dispatch from ModelConfig.enable_cache

    def _split_system(self, messages: list[dict]) -> tuple[list[dict] | str, list[dict]]:
        """Extract system message. Returns cacheable blocks if cache enabled, else plain string."""
        system_parts = []
        user_msgs = []
        for m in messages:
            if m["role"] == "system":
                system_parts.append(m.get("content") or "")
            else:
                user_msgs.append(m)

        system_text = "\n".join(system_parts).strip() if system_parts else "You are a helpful assistant."

        if not self.cache_enabled:
            return system_text, user_msgs

        system_blocks = [
            {"type": "text", "text": system_text, "cache_control": {"type": "ephemeral"}}
        ]
        return system_blocks, user_msgs

    @staticmethod
    def _extract_cache_usage(usage) -> tuple[int, int]:
        """Extract cache_read and cache_creation tokens from Anthropic usage."""
        cache_read = getattr(usage, "cache_read_input_tokens", 0) or 0
        cache_creation = getattr(usage, "cache_creation_input_tokens", 0) or 0
        return cache_read, cache_creation

    def complete(self, messages, model, temperature, max_tokens) -> LLMResponse:
        system_blocks, msgs = self._split_system(messages)
        resp = self._with_retry(
            lambda: self._client.messages.create(
                model=model,
                system=system_blocks,
                messages=msgs,
                temperature=temperature,
                max_tokens=max_tokens or 2000,
            )
        )
        text = "".join(b.text for b in resp.content if b.type == "text")
        cache_read, cache_creation = self._extract_cache_usage(resp.usage)
        return LLMResponse(
            content=text,
            model=resp.model,
            provider=self.provider,
            tokens_prompt=resp.usage.input_tokens,
            tokens_completion=resp.usage.output_tokens,
            tokens_total=resp.usage.input_tokens + resp.usage.output_tokens,
            latency_ms=0,
            cost_usd=0.0,
            cache_read_tokens=cache_read,
            cache_creation_tokens=cache_creation,
        )

    def complete_stream(self, messages, model, temperature, max_tokens):
        system_blocks, msgs = self._split_system(messages)
        with self._client.messages.stream(
            model=model,
            system=system_blocks,
            messages=msgs,
            temperature=temperature,
            max_tokens=max_tokens or 2000,
        ) as stream:
            for event in stream:
                if not hasattr(event, "type"):
                    continue
                if event.type == "content_block_delta":
                    delta = event.delta
                    # Extended thinking
                    if getattr(delta, "type", None) == "thinking_delta":
                        yield {"type": "reasoning", "content": delta.thinking}
                    # Normal text
                    elif hasattr(delta, "text"):
                        yield {"type": "text", "content": delta.text}
            final = stream.get_final_message()
            cache_read, cache_creation = self._extract_cache_usage(final.usage)
            yield {
                "type": "usage",
                "prompt": final.usage.input_tokens,
                "completion": final.usage.output_tokens,
                "cache_read": cache_read,
                "cache_creation": cache_creation,
            }

    def complete_with_tools(
        self,
        messages: list[dict],
        tools: list[dict],
        model: str,
        tool_choice: str,
        temperature: float,
        max_tokens: int | None,
    ) -> dict[str, Any]:
        system_blocks, msgs = self._split_system(messages)
        anthropic_tools = self._convert_tools_with_cache(tools)
        resp = self._with_retry(
            lambda: self._client.messages.create(
                model=model,
                system=system_blocks,
                messages=msgs,
                tools=anthropic_tools,
                temperature=temperature,
                max_tokens=max_tokens or 2000,
            )
        )
        cache_read, cache_creation = self._extract_cache_usage(resp.usage)
        result: dict[str, Any] = {
            "content": "",
            "tool_calls": [],
            "usage": {
                "prompt_tokens": resp.usage.input_tokens,
                "completion_tokens": resp.usage.output_tokens,
                "total_tokens": resp.usage.input_tokens + resp.usage.output_tokens,
                "cache_read_tokens": cache_read,
                "cache_creation_tokens": cache_creation,
            },
        }
        import json as _json

        for block in resp.content:
            if block.type == "text":
                result["content"] += block.text
            elif block.type == "tool_use":
                result["tool_calls"].append(
                    {
                        "id": block.id,
                        "type": "function",
                        "function": {"name": block.name, "arguments": _json.dumps(block.input)},
                    }
                )
        return result

    def complete_with_tools_stream(
        self, messages, tools, model, tool_choice, temperature, max_tokens
    ):
        system_blocks, msgs = self._split_system(messages)
        anthropic_tools = self._convert_tools_with_cache(tools)
        import json as _json

        with self._client.messages.stream(
            model=model,
            system=system_blocks,
            messages=msgs,
            tools=anthropic_tools,
            temperature=temperature,
            max_tokens=max_tokens or 2000,
        ) as stream:
            for event in stream:
                if hasattr(event, "type") and event.type == "content_block_delta":
                    delta = event.delta
                    if getattr(delta, "type", None) == "thinking_delta":
                        yield {"type": "reasoning", "content": delta.thinking}
                    elif hasattr(delta, "text"):
                        yield {"type": "text", "content": event.delta.text}
            msg = stream.get_final_message()
            for block in msg.content:
                if block.type == "tool_use":
                    yield {
                        "type": "tool_call",
                        "data": {
                            "id": block.id,
                            "type": "function",
                            "function": {"name": block.name, "arguments": _json.dumps(block.input)},
                        },
                    }
            cache_read, cache_creation = self._extract_cache_usage(msg.usage)
            yield {
                "type": "usage",
                "prompt": msg.usage.input_tokens,
                "completion": msg.usage.output_tokens,
                "cache_read": cache_read,
                "cache_creation": cache_creation,
            }

    def _convert_tools_with_cache(self, tools: list[dict]) -> list[dict]:
        """Convert OpenAI tools to Anthropic format; mark last tool as cacheable if cache enabled."""
        anthropic_tools = [self._convert_tool(t) for t in tools]
        if anthropic_tools and self.cache_enabled:
            anthropic_tools[-1]["cache_control"] = {"type": "ephemeral"}
        return anthropic_tools

    @staticmethod
    def _convert_tool(openai_tool: dict) -> dict:
        """Convert OpenAI tool format to Anthropic format."""
        fn = openai_tool.get("function", openai_tool)
        return {
            "name": fn["name"],
            "description": fn.get("description", ""),
            "input_schema": fn.get("parameters", {"type": "object", "properties": {}}),
        }


class MockEchoProvider(BaseProvider):
    """Mock provider that echoes back the user message. For testing only."""
    
    provider = LLMProvider.MOCK
    
    def __init__(self):
        pass
    
    def complete(self, messages: list[dict], model: str, temperature: float, max_tokens: int | None) -> LLMResponse:
        # Echo the last user message
        user_msg = next((m["content"] for m in reversed(messages) if m["role"] == "user"), "")
        return LLMResponse(
            content=f"Echo: {user_msg}",
            model=model,
            provider=self.provider,
            tokens_prompt=10,
            tokens_completion=10,
            tokens_total=20,
            latency_ms=1,
            cost_usd=0.0,
        )
    
    def complete_stream(self, messages: list[dict], model: str, temperature: float, max_tokens: int | None) -> Iterator[dict]:
        user_msg = next((m["content"] for m in reversed(messages) if m["role"] == "user"), "")
        response = f"Echo: {user_msg}"
        
        for word in response.split():
            yield {"type": "text", "content": word + " "}
        
        yield {"type": "usage", "prompt": 10, "completion": 10}
    
    def complete_with_tools(
        self,
        messages: list[dict],
        tools: list[dict],
        tool_choice: str,
        model: str,
        temperature: float,
        max_tokens: int | None,
    ) -> dict:
        # Echo without tool calls
        user_msg = next((m["content"] for m in reversed(messages) if m["role"] == "user"), "")
        return {
            "content": f"Echo: {user_msg}",
            "tool_calls": [],
            "model": model,
            "usage": {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20},
        }
    
    def complete_with_tools_stream(
        self,
        messages: list[dict],
        tools: list[dict],
        tool_choice: str,
        model: str,
        temperature: float,
        max_tokens: int | None,
    ) -> Iterator[dict]:
        user_msg = next((m["content"] for m in reversed(messages) if m["role"] == "user"), "")
        response = f"Echo: {user_msg}"
        
        for word in response.split():
            yield {"type": "text", "content": word + " "}
        
        yield {"type": "usage", "prompt": 10, "completion": 10}


