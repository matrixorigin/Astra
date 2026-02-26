# This module has been removed.
# Session-level replay is now handled by api.services.replay_service.ReplayService.
# See plans/replay-system-refactoring-2026-02-26.md for details.
raise ImportError(
    "core.replay.engine has been removed. "
    "Use api.services.replay_service.ReplayService instead."
)
