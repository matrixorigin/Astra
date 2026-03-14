"""Skill discovery tool for LLM to find relevant skills.

Enables LLM to discover skills from a large catalog (1000+) without
listing them all in the system prompt. Uses semantic search over
skill embeddings with keyword fallback.
"""

import logging
import os
from collections.abc import Callable
from typing import Any, ClassVar

from cli.tools.base import EdgeTool, SideEffect

logger = logging.getLogger(__name__)


class FindSkillsTool(EdgeTool):
    """Search for skills that can help with a task.

    Use when you need capabilities not in your current tool set.
    Returns top matches with descriptions and relevance scores.
    """

    name = "find_skills"
    description = (
        "Search for additional skills by task description. "
        "Use only when you need a capability not in your current tool list. "
        "Once you find a matching skill, call it directly by name — do NOT call find_skills again."
    )
    parameters: ClassVar[dict[str, Any]] = {
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Task description or capability to search for (e.g. 'CI failure analysis', 'GitHub PRs')",
            },
            "category": {
                "type": "string",
                "description": "Optional category filter (e.g. 'github', 'aws', 'monitoring')",
            },
            "limit": {
                "type": "integer",
                "description": "Maximum number of results (default: 5)",
                "default": 5,
            },
        },
        "required": ["query"],
    }
    side_effect = SideEffect.READ

    def _get_embed_fn(self) -> Callable[[str], list[float]] | None:
        """Get embedding function from configured provider.

        Returns None if:
        - Embedding provider not configured
        - API key missing
        - Provider initialization fails

        Caller should fall back to keyword search when None.
        """
        try:
            from api.models._constants import EMBEDDING_DIM
            from core.embedding.client import EmbeddingClient

            provider = os.getenv("EMBEDDING_PROVIDER", "openai")
            model = os.getenv("EMBEDDING_MODEL", "text-embedding-3-small")
            api_key = os.getenv("OPENAI_API_KEY", "")

            if provider == "openai" and not api_key:
                logger.debug("Embedding API key not configured, will use keyword search")
                return None

            client = EmbeddingClient(
                provider=provider,
                model=model,
                dim=EMBEDDING_DIM,
                api_key=api_key,
            )
            return client.embed
        except Exception as e:
            logger.debug("Failed to initialize embedding client: %s, will use keyword search", e)
            return None

    async def execute(
        self, query: str, category: str | None = None, limit: int = 5, **kwargs: Any
    ) -> str:
        """Search skills using semantic index with keyword fallback.

        Strategy:
        1. Try semantic search if embedding function available
        2. Fall back to keyword search if semantic search unavailable or returns no results
        3. Return formatted results for LLM consumption
        """
        try:
            from api.database import get_db_session
            from core.skills.skill_index import SkillIndex

            # Get embedding function if available (may be None)
            embed_fn = self._get_embed_fn()

            def _db_factory():
                return next(get_db_session())

            index = SkillIndex(embed_fn=embed_fn, db_factory=_db_factory)

            # Query returns skill names; we need to fetch descriptions
            skill_names = index.query(query, top_k=limit * 2, category=category)

            if not skill_names:
                # Fallback: keyword search in DB when semantic search returns nothing
                logger.debug(
                    "Semantic search returned no results for query '%s', falling back to keyword search",
                    query,
                )
                return await self._keyword_search(query, category, limit)

            # Fetch skill details
            results = await self._fetch_skill_details(skill_names[:limit])

            if not results:
                logger.debug("No skill details found for semantic results: %s", skill_names[:limit])
                return (
                    f"No skills found matching '{query}'. "
                    "You can use bash to accomplish this task instead "
                    "(e.g. gh, curl, git commands)."
                )

            # Format output
            lines = [f"Found {len(results)} skills matching '{query}':"]
            for r in results:
                lines.append(f"\n**{r['name']}**")
                if r.get("description"):
                    # Truncate long descriptions to 200 chars with indicator
                    desc = r["description"]
                    if len(desc) > 200:
                        desc = desc[:197] + "..."
                    lines.append(f"  {desc}")
                if r.get("category"):
                    lines.append(f"  Category: {r['category']}")

            lines.append(
                "\nCall these skills directly by name. Use get_agent_info(dimension='capability') only if you need parameter details."
            )
            return "\n".join(lines)

        except Exception as e:
            logger.error("Skill search failed: %s", e, exc_info=True)
            return f"Skill search failed: {e}"

    async def _keyword_search(self, query: str, category: str | None, limit: int) -> str:
        """Fallback keyword search when semantic search unavailable.

        Performs simple substring matching on skill names and descriptions.
        Scores by: name match (2x weight) > description match (1x weight).
        """
        try:
            from api.database import get_db_session
            from api.models import SkillRegistry

            db = next(get_db_session())
            try:
                q = db.query(
                    SkillRegistry.skill_name,
                    SkillRegistry.description,
                    SkillRegistry.category,
                ).filter(SkillRegistry.is_active == 1)

                if category:
                    q = q.filter(SkillRegistry.category == category)

                rows = q.limit(100).all()

                # Word-level bidirectional matching: each alpha word (3+ chars) from
                # query is checked against skill name parts bidirectionally.
                # e.g. "github issues" → words ["github","issues"]
                #      "issues" ↔ "list_issues" → match
                import re as _re

                query_words = _re.findall(r"[a-z]{3,}", query.lower())
                # Also capture short (2-char) pure-alpha tokens when the whole
                # query is short (e.g. "ci", "go") — avoids UUID fragment noise
                # from long queries like "nonexistent_<uuid>"
                if not query_words:
                    query_words = _re.findall(r"[a-z]{2,}", query.lower())
                # Also include full CJK/non-ascii tokens as single units
                cjk_tokens = _re.findall(r"[^\x00-\x7f]+", query.lower())
                query_tokens = query_words + cjk_tokens

                matches = []
                for name, desc, cat in rows:
                    score = 0
                    name_lower = name.lower()
                    desc_lower = (desc or "").lower()
                    for tok in query_tokens:
                        # bidirectional: tok in name part OR name part in tok
                        if tok in name_lower or name_lower in tok:
                            score += 2
                        elif desc and len(tok) >= 5 and (tok in desc_lower or desc_lower in tok):
                            score += 1
                    # System/meta skills matched only via description are
                    # deprioritised — they appear in results only when the
                    # user explicitly asks about configuration/system topics
                    if score > 0 and cat == "system" and score < 2:
                        score = 0
                    if score > 0:
                        matches.append((name, desc, cat, score))

                matches.sort(key=lambda x: x[3], reverse=True)
                matches = matches[:limit]

                if not matches:
                    logger.debug("Keyword search found no matches for query '%s'", query)
                    return (
                        f"No skills found matching '{query}'. "
                        "You can use bash to accomplish this task instead "
                        "(e.g. gh, curl, git commands)."
                    )

                lines = [f"Found {len(matches)} skills matching '{query}':"]
                for name, desc, cat, _ in matches:
                    lines.append(f"\n**{name}**")
                    if desc:
                        # Truncate with indicator
                        desc_short = desc[:200] + "..." if len(desc) > 200 else desc
                        lines.append(f"  {desc_short}")
                    if cat:
                        lines.append(f"  Category: {cat}")

                lines.append(
                    "\nCall these skills directly by name. Use get_agent_info(dimension='capability') only if you need parameter details."
                )
                return "\n".join(lines)
            finally:
                db.close()

        except Exception as e:
            logger.error("Keyword search failed: %s", e, exc_info=True)
            return f"Keyword search failed: {e}"

    async def _fetch_skill_details(self, skill_names: list[str]) -> list[dict[str, Any]]:
        """Fetch skill details from database.

        Preserves order from semantic search ranking.
        Skips nonexistent or inactive skills.
        """
        if not skill_names:
            return []

        try:
            from api.database import get_db_session
            from api.models import SkillRegistry

            db = next(get_db_session())
            try:
                rows = (
                    db.query(
                        SkillRegistry.skill_name,
                        SkillRegistry.description,
                        SkillRegistry.category,
                    )
                    .filter(
                        SkillRegistry.skill_name.in_(skill_names),
                        SkillRegistry.is_active == 1,
                    )
                    .all()
                )

                # Preserve order from semantic search
                name_to_row = {r[0]: r for r in rows}
                results = []
                for name in skill_names:
                    if name in name_to_row:
                        row = name_to_row[name]
                        results.append(
                            {
                                "name": row[0],
                                "description": row[1],
                                "category": row[2],
                            }
                        )
                return results
            finally:
                db.close()

        except Exception:
            return []
