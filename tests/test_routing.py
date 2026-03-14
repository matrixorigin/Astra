"""Test the query routing system."""

import pytest
from core.routing import RoutingService, AgentType


def test_code_routing():
    """Test routing of code-related queries."""
    service = RoutingService()

    # Test Python code query
    decision = service.route_query("Write a Python function to parse JSON files")
    assert decision.routing_result.agent_type == AgentType.CODE
    assert decision.routing_result.confidence > 0.3
    assert decision.agent_config.temperature == 0.3
    assert "read_file" in decision.agent_config.preferred_tools

    # Test code review query
    decision = service.route_query("Review this JavaScript code for bugs")
    assert decision.routing_result.agent_type == AgentType.CODE


def test_planning_routing():
    """Test routing of planning-related queries."""
    service = RoutingService()

    decision = service.route_query("Create a project plan for building a web application")
    assert decision.routing_result.agent_type == AgentType.PLANNING
    assert decision.routing_result.confidence > 0.3
    assert decision.agent_config.temperature == 0.5

    decision = service.route_query("Design the architecture for a microservices system")
    assert decision.routing_result.agent_type == AgentType.PLANNING


def test_debugging_routing():
    """Test routing of debugging-related queries."""
    service = RoutingService()

    decision = service.route_query(
        "Fix this error: TypeError: 'NoneType' object is not subscriptable"
    )
    assert decision.routing_result.agent_type == AgentType.DEBUGGING
    assert decision.routing_result.confidence > 0.3
    assert decision.agent_config.temperature == 0.2

    decision = service.route_query("My application is crashing, help me debug it")
    assert decision.routing_result.agent_type == AgentType.DEBUGGING


def test_general_routing():
    """Test routing of general queries."""
    service = RoutingService()

    decision = service.route_query("What is the weather like today?")
    assert decision.routing_result.agent_type == AgentType.GENERAL
    assert decision.agent_config.temperature == 0.7

    decision = service.route_query("Explain quantum computing")
    assert decision.routing_result.agent_type == AgentType.GENERAL


def test_context_modifications():
    """Test that routing properly modifies context."""
    service = RoutingService()

    original_context = {"model": "gpt-4", "custom_field": "value"}
    decision = service.route_query("Debug this Python error", original_context)

    # Should preserve original context
    assert decision.context_modifications["custom_field"] == "value"
    assert decision.context_modifications["model"] == "gpt-4"

    # Should add routing information
    assert decision.context_modifications["agent_type"] == "debugging"
    assert decision.context_modifications["temperature"] == 0.2
    assert "routing_confidence" in decision.context_modifications
    assert "system_prompt" in decision.context_modifications


def test_empty_query():
    """Test handling of empty queries."""
    service = RoutingService()

    decision = service.route_query("")
    assert decision.routing_result.agent_type == AgentType.GENERAL
    assert decision.routing_result.confidence == 1.0


if __name__ == "__main__":
    # Run basic tests
    test_code_routing()
    test_planning_routing()
    test_debugging_routing()
    test_general_routing()
    test_context_modifications()
    test_empty_query()
    print("All routing tests passed!")
