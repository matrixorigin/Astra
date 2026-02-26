"""Replay package for conversation reproduction.

Core components:
- TimeMachine: Time-travel via MatrixOne snapshots
- SemanticDiff: Compare agent behaviors across sessions/checkpoints
- StreamReplay: Reconstruct streams from logged events (in core/agent/)

Session-level replay is handled by api.services.replay_service.ReplayService.
"""
