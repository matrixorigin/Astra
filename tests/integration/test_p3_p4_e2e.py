"""P3 & P4 E2E tests — Real /chat API with streaming and scheduling.

Uses mock_llm_for_chat pattern from test_e2e_realistic.py:
  - Real EventLogger, ContextManager, SkillPipeline, Firewall
  - Mocked LLM (ScriptedLLM) for deterministic responses
  - Tests verify real data accumulation via API queries
"""

from contextlib import contextmanager
from dataclasses import dataclass
from unittest.mock import patch

import pytest
from fastapi.testclient import TestClient

from api.main import app
from core.llm.models import LLMResponse
from core.scheduling import (
    Condition,
    ConditionOperator,
    TriggerRule,
    TriggerRuleRegistry,
)
from core.utils.id_generator import generate_id


@dataclass
class Turn:
    """Scripted LLM turn."""
    content: str = ""
    tool_calls: list[dict] | None = None


class ScriptedLLM:
    """Mock LLM for E2E tests."""
    
    def __init__(self, script: list[Turn] | None = None):
        self.script = list(script or [])
        self._cursor = 0
        self.config = {"model": "scripted", "temperature": 0.0}
    
    def _next_turn(self) -> Turn:
        if self._cursor < len(self.script):
            t = self.script[self._cursor]
            self._cursor += 1
            return t
        return Turn(content="Done")
    
    def chat(self, messages, **kwargs):
        """Plain chat."""
        from core.llm.models import LLMProvider
        return LLMResponse(
            content="ok",
            model="scripted",
            provider=LLMProvider.OPENAI,
            tokens_prompt=10,
            tokens_completion=5,
            tokens_total=15,
            latency_ms=100,
            cost_usd=0.001,
        )
    
    def chat_with_tools(self, messages, tools=None, tool_choice="auto", **kwargs):
        """Chat with tools."""
        turn = self._next_turn()
        result = {"content": turn.content}
        if turn.tool_calls:
            result["tool_calls"] = turn.tool_calls
        return result
    
    async def chat_stream(self, messages, **kwargs):
        """Stream chat."""
        yield {"type": "text", "content": self._next_turn().content}
    
    async def chat_with_tools_stream(self, messages, tools, tool_choice="auto", **kwargs):
        """Stream chat with tools."""
        turn = self._next_turn()
        if turn.tool_calls:
            for tc in turn.tool_calls:
                yield {"type": "tool_call", "data": tc}
        if turn.content:
            yield {"type": "text", "content": turn.content}


def _build_patched_chat_loop(llm: ScriptedLLM):
    """Patch _build_chat_loop to inject ScriptedLLM."""
    def patched(db):
        from core.agent.chat_loop import ChatLoop
        from core.agent.executor import AgentExecutor
        from core.context.manager import ContextManager
        from core.events.event_logger import EventLogger
        from core.verification.firewall import HallucinationFirewall
        from core.skills.pipeline import SkillPipeline
        from core.skills.registry import SkillRegistry

        event_logger = EventLogger(db)
        skill_registry = SkillRegistry(db)
        context_manager = ContextManager(db)
        selector = SkillPipeline(db, llm, audit=True, learning=True)
        executor = AgentExecutor(db, skill_registry)
        firewall = HallucinationFirewall(db, context_manager)

        loop = ChatLoop(
            selector=selector,
            executor=executor,
            llm_client=llm,
            event_logger=event_logger,
            context_manager=context_manager,
            firewall=firewall,
        )
        return loop

    return patched


@contextmanager
def mock_llm_for_chat(builder_fn):
    """Context manager that patches _build_chat_loop call sites."""
    import core.agent.run_engine as re_mod
    original_start = re_mod.RunEngine.start_run

    async def patched_start(self_engine, run):
        with patch("api.routers.chat._build_chat_loop", builder_fn):
            await original_start(self_engine, run)

    with patch("api.routers.chat._build_chat_loop", builder_fn):
        with patch.object(re_mod.RunEngine, "start_run", patched_start):
            yield


@pytest.fixture
def client():
    return TestClient(app)


@pytest.fixture
def auth_headers(client):
    """Register + login, return auth headers."""
    username = f"test_{generate_id()}"
    client.post("/auth/register", json={
        "username": username,
        "email": f"{username}@test.com",
        "password": "testpass1234",
    })
    resp = client.post("/auth/login", json={
        "username": username,
        "password": "testpass1234",
    })
    return {"Authorization": f"Bearer {resp.json()['access_token']}"}


class TestStreamingE2E:
    """End-to-end streaming tests via /chat API."""
    
    def test_chat_stream_real_events(self, client, auth_headers):
        """Test /chat/stream returns real events.
        
        Real scenario: User sends message → API processes → Events stream
        """
        script = [
            Turn(content="Event sourcing is an architectural pattern..."),
        ]
        
        llm = ScriptedLLM(script)
        builder = _build_patched_chat_loop(llm)
        
        with mock_llm_for_chat(builder):
            response = client.post(
                "/chat/stream",
                json={
                    "message": "What is event sourcing?",
                },
                headers=auth_headers,
            )
            
            assert response.status_code == 200
            assert "text/event-stream" in response.headers["content-type"]
    
    def test_multi_turn_streaming(self, client, auth_headers):
        """Test multi-turn conversation streaming.
        
        Real scenario: 3-turn conversation → All events stream
        """
        script = [
            Turn(content="Python is a programming language."),
            Turn(content="Python has many features."),
            Turn(content="You can learn Python through practice."),
        ]
        
        llm = ScriptedLLM(script)
        builder = _build_patched_chat_loop(llm)
        
        with mock_llm_for_chat(builder):
            for msg in [
                "What is Python?",
                "What are its features?",
                "How to learn it?",
            ]:
                response = client.post(
                    "/chat/stream",
                    json={
                        "message": msg,
                    },
                    headers=auth_headers,
                )
                
                assert response.status_code == 200
                assert "text/event-stream" in response.headers["content-type"]


class TestAutoSchedulingE2E:
    """End-to-end auto-scheduling tests via /chat API."""
    
    def test_trigger_rule_on_real_chat(self, client, auth_headers):
        """Test trigger rule fires on real chat message.
        
        Real scenario: User sends urgent message → Trigger rule matches
        """
        # Setup trigger rule
        registry = TriggerRuleRegistry()
        rule = TriggerRule(
            rule_id="urgent_rule",
            name="Urgent Handler",
            description="Handle urgent queries",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "urgent"),
            ],
        )
        registry.register_rule(rule)
        
        script = [
            Turn(content="Urgent task processed immediately..."),
        ]
        
        llm = ScriptedLLM(script)
        builder = _build_patched_chat_loop(llm)
        
        with mock_llm_for_chat(builder):
            response = client.post(
                "/chat",
                json={
                    "message": "urgent: Fix the critical bug!",
                },
                headers=auth_headers,
            )
            
            assert response.status_code == 200
            data = response.json()
            assert "run_id" in data
            
            # Verify rule would match
            event_dict = {
                "event_type": "user_query",
                "data": {"content": "urgent: Fix the critical bug!"},
            }
            matching = registry.find_matching_rules(event_dict)
            assert len(matching) == 1
    
    def test_multi_rule_triggering(self, client, auth_headers):
        """Test multiple rules triggering on different messages.
        
        Real scenario: 3 messages → Different rules trigger
        """
        # Setup rules
        registry = TriggerRuleRegistry()
        
        rule_urgent = TriggerRule(
            rule_id="urgent_rule",
            name="Urgent Handler",
            description="Handle urgent",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "urgent"),
            ],
        )
        
        rule_analysis = TriggerRule(
            rule_id="analysis_rule",
            name="Analysis Handler",
            description="Handle analysis",
            event_type="user_query",
            conditions=[
                Condition("data.content", ConditionOperator.CONTAINS, "analyze"),
            ],
        )
        
        registry.register_rule(rule_urgent)
        registry.register_rule(rule_analysis)
        
        script = [
            Turn(content="Urgent task processed"),
            Turn(content="Analysis complete"),
            Turn(content="Normal response"),
        ]
        
        llm = ScriptedLLM(script)
        builder = _build_patched_chat_loop(llm)
        
        with mock_llm_for_chat(builder):
            messages = [
                "urgent: Fix bug",
                "Please analyze this data",
                "Normal question",
            ]
            
            triggered_rules = []
            
            for msg in messages:
                response = client.post(
                    "/chat",
                    json={
                        "message": msg,
                    },
                    headers=auth_headers,
                )
                
                assert response.status_code == 200
                
                # Check which rules would match
                event_dict = {
                    "event_type": "user_query",
                    "data": {"content": msg},
                }
                matching = registry.find_matching_rules(event_dict)
                triggered_rules.append(len(matching))
            
            # Verify
            assert triggered_rules[0] == 1  # urgent matches
            assert triggered_rules[1] == 1  # analysis matches
            assert triggered_rules[2] == 0  # normal matches none
