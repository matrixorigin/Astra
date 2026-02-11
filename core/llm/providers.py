"""LLM provider adapters with connection pooling and retry."""

import time
import logging
from abc import ABC, abstractmethod
from typing import Any, Iterator

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
    for code in (429, 500, 502, 503, 504):
        if str(code) in str(error):
            return True
    return False


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
                    time.sleep(RETRY_BASE_DELAY * (2**attempt))
                    logger.warning(f"Retry {attempt + 1}/{MAX_RETRIES}: {e}")
                else:
                    raise
        raise last_error


def _accumulate_tool_calls(response_iter) -> Iterator[dict]:
    """Shared tool call accumulation for OpenAI-compatible streaming."""
    buf: dict[int, dict] = {}
    for chunk in response_iter:
        if chunk.usage:
            yield {
                "type": "usage",
                "prompt": chunk.usage.prompt_tokens,
                "completion": chunk.usage.completion_tokens,
            }
            continue
        if not chunk.choices:
            continue
        delta = chunk.choices[0].delta
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
                    buf[idx]["function"]["name"] = tc.function.name
                if tc.function and tc.function.arguments:
                    buf[idx]["function"]["arguments"] += tc.function.arguments
    for tc in buf.values():
        if tc["function"]["name"]:
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
    }


class OpenAIProvider(BaseProvider):
    """OpenAI provider with connection pooling."""

    provider = LLMProvider.OPENAI

    def __init__(self, api_key: str, base_url: str | None = None):
        import openai

        kwargs: dict[str, Any] = {"api_key": api_key}
        if base_url:
            kwargs["base_url"] = base_url
        self._client = openai.OpenAI(**kwargs)

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
                stream_options={"include_usage": True},
            )
        )
        for chunk in resp:
            if chunk.usage:
                yield {
                    "type": "usage",
                    "prompt": chunk.usage.prompt_tokens,
                    "completion": chunk.usage.completion_tokens,
                }
            elif chunk.choices and chunk.choices[0].delta.content:
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
                stream_options={"include_usage": True},
            )
        )
        yield from _accumulate_tool_calls(resp)


class GroqProvider(BaseProvider):
    """Groq provider — OpenAI-compatible API."""

    provider = LLMProvider.GROQ

    def __init__(self, api_key: str):
        from groq import Groq

        self._client = Groq(api_key=api_key)

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
    """Anthropic provider — different API shape, normalized to common interface."""

    provider = LLMProvider.ANTHROPIC

    def __init__(self, api_key: str):
        import anthropic

        self._client = anthropic.Anthropic(api_key=api_key)

    def _split_system(self, messages: list[dict]) -> tuple[str, list[dict]]:
        """Extract system message (Anthropic takes it as a separate param)."""
        system = ""
        user_msgs = []
        for m in messages:
            if m["role"] == "system":
                system += (m.get("content") or "") + "\n"
            else:
                user_msgs.append(m)
        return system.strip(), user_msgs

    def complete(self, messages, model, temperature, max_tokens) -> LLMResponse:
        system, msgs = self._split_system(messages)
        resp = self._with_retry(
            lambda: self._client.messages.create(
                model=model,
                system=system or "You are a helpful assistant.",
                messages=msgs,
                temperature=temperature,
                max_tokens=max_tokens or 2000,
            )
        )
        text = "".join(b.text for b in resp.content if b.type == "text")
        return LLMResponse(
            content=text,
            model=resp.model,
            provider=self.provider,
            tokens_prompt=resp.usage.input_tokens,
            tokens_completion=resp.usage.output_tokens,
            tokens_total=resp.usage.input_tokens + resp.usage.output_tokens,
            latency_ms=0,
            cost_usd=0.0,
        )

    def complete_stream(self, messages, model, temperature, max_tokens):
        system, msgs = self._split_system(messages)
        with self._client.messages.stream(
            model=model,
            system=system or "You are a helpful assistant.",
            messages=msgs,
            temperature=temperature,
            max_tokens=max_tokens or 2000,
        ) as stream:
            for text in stream.text_stream:
                yield {"type": "text", "content": text}
            usage = stream.get_final_message().usage
            yield {"type": "usage", "prompt": usage.input_tokens, "completion": usage.output_tokens}

    def complete_with_tools(self, messages, tools, model, tool_choice, temperature, max_tokens):
        system, msgs = self._split_system(messages)
        # Convert OpenAI tool format → Anthropic tool format
        anthropic_tools = [self._convert_tool(t) for t in tools]
        resp = self._with_retry(
            lambda: self._client.messages.create(
                model=model,
                system=system or "You are a helpful assistant.",
                messages=msgs,
                tools=anthropic_tools,
                temperature=temperature,
                max_tokens=max_tokens or 2000,
            )
        )
        result: dict[str, Any] = {
            "content": "",
            "tool_calls": [],
            "usage": {
                "prompt_tokens": resp.usage.input_tokens,
                "completion_tokens": resp.usage.output_tokens,
                "total_tokens": resp.usage.input_tokens + resp.usage.output_tokens,
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
        system, msgs = self._split_system(messages)
        anthropic_tools = [self._convert_tool(t) for t in tools]
        import json as _json

        with self._client.messages.stream(
            model=model,
            system=system or "You are a helpful assistant.",
            messages=msgs,
            tools=anthropic_tools,
            temperature=temperature,
            max_tokens=max_tokens or 2000,
        ) as stream:
            for event in stream:
                if hasattr(event, "type"):
                    if event.type == "content_block_delta":
                        if hasattr(event.delta, "text"):
                            yield {"type": "text", "content": event.delta.text}
            # After stream, extract tool calls from final message
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
            yield {
                "type": "usage",
                "prompt": msg.usage.input_tokens,
                "completion": msg.usage.output_tokens,
            }

    @staticmethod
    def _convert_tool(openai_tool: dict) -> dict:
        """Convert OpenAI tool format to Anthropic format."""
        fn = openai_tool.get("function", openai_tool)
        return {
            "name": fn["name"],
            "description": fn.get("description", ""),
            "input_schema": fn.get("parameters", {"type": "object", "properties": {}}),
        }
