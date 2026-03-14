"""Zone-based token budget management.

Implements the zone budget system from context-window-management.md:
- Fixed zone: Identity, self-model, project, rules
- Managed zone: Memory, working memory, tool hints
- Elastic zone: Conversation history

P1 Critical Fix: Uses effective_context (model_size - response_reserve) as scale basis.
"""

from __future__ import annotations

from dataclasses import dataclass


# Base budgets optimized for 32K context models
BASE_BUDGETS = {
    "fixed": 4000,  # Identity, self-model, project, rules
    "managed": 3000,  # Memory, working memory, tool hints
    "elastic": 8000,  # Conversation history
    "response_reserve": 4000,  # Reserved for LLM response
}


@dataclass
class ZoneBudgets:
    """Token budgets for each zone."""

    fixed: int
    managed: int
    elastic: int
    response_reserve: int
    total_allocated: int
    effective_context: int
    model_context_size: int


def compute_zone_budgets(model_context_size: int) -> ZoneBudgets:
    """
    Compute zone budgets based on model context size.

    P1 CRITICAL FIX: Use effective_context to determine scale.
    effective_context = model_size - BASE_response_reserve (4K)

    This prevents over-allocation by basing scale on available space, not total size.

    Args:
        model_context_size: Maximum context window size of the model

    Returns:
        ZoneBudgets with allocated token counts for each zone
    """
    # P1: Calculate effective context FIRST
    base_response = BASE_BUDGETS["response_reserve"]
    effective_context = model_context_size - base_response

    # Determine scale based on EFFECTIVE context
    if effective_context < 16000:  # 12K effective -> scale 1.0
        scale = 1.0
    elif effective_context < 32000:  # 16K-28K effective -> scale 1.5
        scale = 1.5
    elif effective_context < 64000:  # 32K-60K effective -> scale 2.0
        scale = 2.0
    else:  # 64K+ effective -> scale 4.0
        scale = 4.0

    # Scale zone budgets (but NOT response reserve)
    fixed = int(BASE_BUDGETS["fixed"] * scale)
    managed = int(BASE_BUDGETS["managed"] * scale)
    elastic = int(BASE_BUDGETS["elastic"] * scale)
    response_reserve = base_response  # Keep fixed at 4K

    total_allocated = fixed + managed + elastic

    return ZoneBudgets(
        fixed=fixed,
        managed=managed,
        elastic=elastic,
        response_reserve=response_reserve,
        total_allocated=total_allocated,
        effective_context=effective_context,
        model_context_size=model_context_size,
    )
