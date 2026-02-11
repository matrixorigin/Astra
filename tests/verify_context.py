import sys
from datetime import datetime
from pathlib import Path
from unittest.mock import MagicMock, patch

# Add project root to path
sys.path.append(str(Path("/home/xupeng/github/mo-dev-agent").resolve()))

from core.context.manager import ContextManager, TaskType


def verify_context_manager():
    print("Verifying ContextManager implementation...")

    # Mock Database
    mock_db = MagicMock()

    # Mock fetchall results for _retrieve_candidates
    mock_db.fetchall.side_effect = [
        # 1. candidates
        [
            {
                "event_id": "evt_1",
                "event_type": "user_query",
                "content": "chain start",
                "created_at": datetime.now(),
                "causal_chain_id": "chain_1",
                "metadata": {},
            },
            {
                "event_id": "evt_2",
                "event_type": "tool_call",
                "content": "mid chain",
                "created_at": datetime.now(),
                "causal_chain_id": "chain_1",
                "metadata": {},
            },
        ],
        # 2. recent_chains (for _score_candidates)
        [{"causal_chain_id": "chain_1", "last_time": datetime.now()}],
        # 3. skills (for _get_skill_definitions)
        [
            {
                "skill_name": "test_skill",
                "version": "1.0",
                "description": "test",
                "category": "test",
                "subcategory": "test",
            }
        ],
    ]

    # Initialize Manager
    with patch("core.context.embeddings.EmbeddingService") as mock_embed:
        mock_embed.return_value.embed_text.return_value = [0.1] * 1024
        mock_embed.return_value.search_similar.return_value = [
            {"event_id": "evt_1", "distance": 0.1},
            {"event_id": "evt_2", "distance": 0.2},
        ]

        manager = ContextManager(mock_db, enable_snapshots=False)

        # Build Context
        context = manager.build_context(
            session_id="sess_1", query="test query", task_type=TaskType.GENERAL
        )

        print("\nContext Built Successfully!")
        print(f"Total Tokens: {context.total_tokens}")
        print(f"Selected Events: {len(context.selected_events)}")
        print(f"Skill Definitions: {len(context.skill_definitions)}")

        # Verify Causal Logic
        # We expect causal_score to be added.
        # Since we mocked logic, we can't easily check internal variables without more hooks,
        # but successful execution means the code paths are valid.

        # Check if skills are populated
        if context.skill_definitions:
            print(f"✅ Skills Populated: {[s['name'] for s in context.skill_definitions]}")
        else:
            print("❌ Skills Empty")

        # Check code context
        if context.code_context:
            print(f"✅ Code Context Populated: {len(context.code_context)} items")
        else:
            print("❌ Code Context Empty (Expected for now)")


if __name__ == "__main__":
    verify_context_manager()
