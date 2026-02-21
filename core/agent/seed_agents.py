"""Seed predefined agent roles for multi-agent workflows."""

import json

SEED_AGENTS = [
    {
        "agent_id": "code-writer",
        "agent_name": "Code Writer",
        "agent_type": "specialist",
        "agent_config": {
            "system_prompt": (
                "You are a code implementation specialist. "
                "Write clean, well-tested code. Follow best practices. "
                "Always explain your design decisions briefly."
            ),
        },
    },
    {
        "agent_id": "security-reviewer",
        "agent_name": "Security Reviewer",
        "agent_type": "reviewer",
        "agent_config": {
            "system_prompt": (
                "You are a security code reviewer. "
                "Focus on: injection vulnerabilities, auth/authz issues, "
                "data exposure, insecure defaults, missing input validation. "
                "Rate severity (critical/high/medium/low). Be specific about line numbers."
            ),
            "allowed_tools": ["read_file", "search_code", "list_directory"],
        },
    },
    {
        "agent_id": "perf-reviewer",
        "agent_name": "Performance Reviewer",
        "agent_type": "reviewer",
        "agent_config": {
            "system_prompt": (
                "You are a performance code reviewer. "
                "Focus on: N+1 queries, unnecessary allocations, missing indexes, "
                "blocking I/O in async code, unbounded collections, cache opportunities. "
                "Suggest concrete fixes with expected impact."
            ),
            "allowed_tools": ["read_file", "search_code", "list_directory"],
        },
    },
    {
        "agent_id": "style-reviewer",
        "agent_name": "Style & Maintainability Reviewer",
        "agent_type": "reviewer",
        "agent_config": {
            "system_prompt": (
                "You are a code style and maintainability reviewer. "
                "Focus on: naming clarity, function length, coupling, "
                "missing docstrings, dead code, inconsistent patterns. "
                "Suggest refactoring only when it meaningfully improves readability."
            ),
            "allowed_tools": ["read_file", "search_code", "list_directory"],
        },
    },
    {
        "agent_id": "orchestrator",
        "agent_name": "Orchestrator",
        "agent_type": "orchestrator",
        "agent_config": {
            "system_prompt": (
                "You are a task orchestrator. Break complex tasks into subtasks "
                "and delegate to specialist agents using spawn_runs. "
                "Synthesize results from child agents into a coherent response. "
                "Available agents: code-writer, security-reviewer, perf-reviewer, style-reviewer."
            ),
        },
    },
]


def seed_agents(db) -> int:
    """Insert seed agents if they don't exist. Returns count of inserted agents."""
    from sqlalchemy import text
    count = 0
    for agent in SEED_AGENTS:
        existing = db.execute(
            text("SELECT 1 FROM agents WHERE agent_id = :aid"),
            {"aid": agent["agent_id"]},
        ).fetchone()
        if existing:
            continue
        db.execute(
            text(
                "INSERT INTO agents (agent_id, agent_name, agent_type, owner_user_id, agent_config) "
                "VALUES (:aid, :name, :type, :owner, :config)"
            ),
            {
                "aid": agent["agent_id"],
                "name": agent["agent_name"],
                "type": agent["agent_type"],
                "owner": "system",
                "config": json.dumps(agent["agent_config"]),
            },
        )
        count += 1
    db.commit()
    return count
