"""Knowledge extraction from conversation events.

Post-chain hook that extracts structured knowledge from completed causal chains.
"""

import json
import re
import uuid
from datetime import datetime
from typing import Any

from core.logging_config import get_logger
from sqlalchemy.orm import Session

logger = get_logger(__name__)


def _normalize_value(v: str) -> str:
    """Normalize a knowledge value for semantic-equivalent comparison.

    Handles casing, whitespace, and common tech synonyms so that
    'TypeScript' vs 'typescript' or 'JS' vs 'JavaScript' are treated
    as the same value.
    """
    v = " ".join(v.lower().split())
    synonyms = {
        "js": "javascript",
        "ts": "typescript",
        "py": "python",
        "golang": "go",
        "k8s": "kubernetes",
        "pg": "postgresql",
        "postgres": "postgresql",
        "mongo": "mongodb",
        "react.js": "react",
        "reactjs": "react",
        "vue.js": "vue",
        "vuejs": "vue",
        "node.js": "node",
        "nodejs": "node",
    }
    return synonyms.get(v, v)


KNOWLEDGE_EXTRACTION_PROMPT = """\
You extract structured knowledge from conversations. Output a JSON array ONLY.

Each item must have:
- "category": one of "user_preference", "codebase_pattern", "domain_fact", "tool_behavior", "entity"
- "key_name": short unique key (e.g. "preferred_language", "auth.pattern")
- "value": the extracted fact

Only extract clear, factual statements. Skip vague or uncertain information.
"""


class KnowledgeExtractor:
    """Extract semantic knowledge from conversation events.
    
    Post-chain hook that analyzes completed causal chains and extracts
    structured knowledge (user preferences, codebase patterns, domain facts).
    
    Features:
    - Pattern-based extraction (MVP, will be LLM-based in production)
    - Confidence decay with exponential formula
    - Batch storage to avoid N+1 queries
    - Trust tier support (T1-T4)
    
    Example:
        >>> extractor = KnowledgeExtractor(db)
        >>> extracted = extractor.extract_from_chain(chain_id, user_id)
        >>> extractor.decay_confidence(user_id, half_life_days=60)
    """
    
    # Case-insensitive patterns
    PREFERENCE_PATTERNS = re.compile(
        r'\b(i prefer|i like|i want|i use|i always|i never)\b',
        re.IGNORECASE
    )
    
    PATTERN_PATTERNS = re.compile(
        r'\b(pattern|architecture|uses|implements|follows)\b',
        re.IGNORECASE
    )

    def __init__(self, db: Session, llm_client=None):
        """Initialize knowledge extractor.
        
        Args:
            db: SQLAlchemy database session
            llm_client: Optional LLM client for extraction (falls back to regex)
        """
        self.db = db
        self.llm = llm_client

    def extract_from_chain(self, causal_chain_id: str, user_id: str) -> list[dict[str, Any]]:
        """Extract knowledge from completed causal chain.
        
        Uses LLM extraction when available, falls back to regex patterns.
        
        Args:
            causal_chain_id: Completed causal chain
            user_id: User who owns the conversation
            
        Returns:
            List of extracted knowledge entries
        """
        from api.models import Event
        
        events = self.db.query(Event).filter(
            Event.causal_chain_id == causal_chain_id,
            Event.user_id == user_id
        ).order_by(Event.created_at).all()
        
        if not events:
            return []
        
        if self.llm:
            extracted = self._extract_via_llm(events, user_id)
        else:
            extracted = self._extract_via_regex(events, user_id)
        
        stored = self._batch_store_knowledge(extracted)
        
        logger.info(f"Extracted {len(stored)} knowledge entries from chain {causal_chain_id}")
        return stored

    def _extract_via_llm(self, events, user_id: str) -> list[dict[str, Any]]:
        """Extract knowledge from events using LLM."""
        from core.memory.observer import _parse_json_array
        from core.context.lifecycle import trust_tier_defaults

        conv_text = "\n".join(
            f"[{e.event_type}]: {e.content[:500]}" for e in events if e.content
        )
        event_ids = [e.event_id for e in events]

        try:
            result = self.llm.chat_with_tools(
                messages=[
                    {"role": "system", "content": KNOWLEDGE_EXTRACTION_PROMPT},
                    {"role": "user", "content": conv_text},
                ],
                tools=[],
                tool_choice="none",
            )
            raw = _parse_json_array(result.get("content", ""))
        except Exception as e:
            logger.warning("Knowledge LLM extraction failed: %s", e)
            return []

        defaults = trust_tier_defaults("T3")
        valid_categories = {"user_preference", "codebase_pattern", "domain_fact", "tool_behavior", "entity"}
        extracted = []
        for item in raw:
            if not isinstance(item, dict) or not item.get("key_name") or not item.get("value"):
                continue
            category = item.get("category", "domain_fact")
            if category not in valid_categories:
                category = "domain_fact"
            extracted.append({
                "user_id": user_id,
                "category": category,
                "key_name": item["key_name"],
                "value": item["value"],
                "source_event_ids": event_ids,
                "extraction_method": "llm_extraction",
                "trust_tier": "T3",
                "confidence": defaults["initial_confidence"],
            })
        return extracted

    def _extract_via_regex(self, events, user_id: str) -> list[dict[str, Any]]:
        """Fallback regex extraction when no LLM available."""
        extracted = []
        for event in events:
            if self.PREFERENCE_PATTERNS.search(event.content):
                entry = self._extract_preference(event, user_id)
                if entry:
                    extracted.append(entry)
            if self.PATTERN_PATTERNS.search(event.content):
                entry = self._extract_pattern(event, user_id)
                if entry:
                    extracted.append(entry)
        return extracted

    def _extract_preference(self, event, user_id: str) -> dict[str, Any] | None:
        """Extract user preference from event."""
        content = event.content
        
        # Simple extraction - in production use LLM
        if "typescript" in content.lower():
            from core.context.lifecycle import trust_tier_defaults
            defaults = trust_tier_defaults("T3")
            return {
                "user_id": user_id,
                "category": "user_preference",
                "key_name": "language",
                "value": "typescript",
                "source_event_ids": [event.event_id],
                "extraction_method": "observation",
                "trust_tier": "T3",
                "confidence": defaults["initial_confidence"],
            }
        return None

    def _extract_pattern(self, event, user_id: str) -> dict[str, Any] | None:
        """Extract codebase pattern from event."""
        content = event.content.lower()
        
        if "dependency injection" in content:
            from core.context.lifecycle import trust_tier_defaults
            defaults = trust_tier_defaults("T3")
            return {
                "user_id": user_id,
                "category": "codebase_pattern",
                "key_name": "auth.pattern",
                "value": "dependency_injection",
                "source_event_ids": [event.event_id],
                "extraction_method": "observation",
                "trust_tier": "T3",
                "confidence": defaults["initial_confidence"],
            }
        return None

    def _batch_store_knowledge(self, entries: list[dict[str, Any]]) -> list[dict[str, Any]]:
        """Batch store knowledge entries to avoid N+1 queries.
        
        Args:
            entries: List of knowledge entries to store
            
        Returns:
            List of stored entry results
        """
        from api.models import KnowledgeEntry
        
        if not entries:
            return []
        
        stored = []
        
        # Group by key for deduplication
        entries_by_key = {}
        for entry in entries:
            key = (entry["user_id"], entry["category"], entry["key_name"])
            entries_by_key[key] = entry
        
        # Check existing entries in batch
        keys_to_check = [(e["user_id"], e["category"], e["key_name"]) for e in entries_by_key.values()]
        
        existing_entries = {}
        if keys_to_check:
            from sqlalchemy import or_, and_
            
            conditions = [
                and_(
                    KnowledgeEntry.user_id == user_id,
                    KnowledgeEntry.category == category,
                    KnowledgeEntry.key_name == key_name
                )
                for user_id, category, key_name in keys_to_check
            ]
            
            existing = self.db.query(KnowledgeEntry).filter(or_(*conditions)).all()
            
            for e in existing:
                key = (e.user_id, e.category, e.key_name)
                existing_entries[key] = e
        
        # Update or create entries
        for key, entry in entries_by_key.items():
            if key in existing_entries:
                existing = existing_entries[key]
                now = datetime.now()
                if _normalize_value(existing.value) == _normalize_value(entry["value"]):
                    # Same value — reinforce confidence
                    existing.confidence = min(1.0, existing.confidence + 0.1)
                    existing.version += 1
                    existing.last_validated_at = now
                    existing.updated_at = now
                    stored.append({
                        "entry_id": existing.entry_id,
                        "action": "updated",
                        "confidence": existing.confidence,
                    })
                else:
                    # Contradiction — decay old, supersede with new
                    logger.warning(
                        "Knowledge contradiction: %s = %r vs %r",
                        entry["key_name"], existing.value, entry["value"],
                    )
                    existing.confidence = max(0, existing.confidence - 0.3)
                    existing.updated_at = now
                    entry_id = str(uuid.uuid4())
                    knowledge = KnowledgeEntry(
                        entry_id=entry_id,
                        user_id=entry["user_id"],
                        category=entry["category"],
                        key_name=entry["key_name"],
                        value=entry["value"],
                        source_event_ids=json.dumps(entry["source_event_ids"]),
                        extraction_method=entry.get("extraction_method", "observation"),
                        trust_tier=entry["trust_tier"],
                        confidence=entry["confidence"],
                        initial_confidence=entry["confidence"],
                    )
                    existing.superseded_by = entry_id
                    self.db.add(knowledge)
                    stored.append({
                        "entry_id": entry_id,
                        "action": "contradiction",
                        "confidence": entry["confidence"],
                    })
            else:
                # Create new
                entry_id = str(uuid.uuid4())
                knowledge = KnowledgeEntry(
                    entry_id=entry_id,
                    user_id=entry["user_id"],
                    category=entry["category"],
                    key_name=entry["key_name"],
                    value=entry["value"],
                    source_event_ids=json.dumps(entry["source_event_ids"]),
                    extraction_method=entry["extraction_method"],
                    trust_tier=entry["trust_tier"],
                    confidence=entry["confidence"],
                    initial_confidence=entry["confidence"],
                )
                
                self.db.add(knowledge)
                
                stored.append({
                    "entry_id": entry_id,
                    "action": "created",
                    "confidence": entry["confidence"]
                })
                logger.info(f"Created knowledge entry: {entry['key_name']} (confidence={entry['confidence']})")
        
        # Single commit for all changes
        self.db.commit()
        
        return stored

    def decay_confidence(self, user_id: str, half_life_days: int = 60) -> int:
        """Apply confidence decay to knowledge entries.
        
        Args:
            user_id: User whose knowledge to decay
            half_life_days: Days for confidence to halve
            
        Returns:
            Number of entries decayed
        """
        from api.models import KnowledgeEntry
        from sqlalchemy import func
        
        # Calculate days since validation
        days_diff = func.datediff(func.now(), KnowledgeEntry.last_validated_at)
        
        # Calculate decay: initial_confidence * 0.5^(days_diff / half_life)
        decay_factor = func.power(0.5, days_diff / half_life_days)
        new_confidence = KnowledgeEntry.initial_confidence * decay_factor
        
        # Update entries with confidence > 0.3
        result = self.db.query(KnowledgeEntry).filter(
            KnowledgeEntry.user_id == user_id,
            KnowledgeEntry.confidence > 0.3
        ).update(
            {
                KnowledgeEntry.confidence: new_confidence,
                KnowledgeEntry.updated_at: func.now()
            },
            synchronize_session=False
        )
        
        self.db.commit()
        
        count = result
        logger.info(f"Applied confidence decay to {count} entries for user {user_id}")
        return count

    def quarantine_low_confidence(self, user_id: str, threshold: float = 0.3) -> int:
        """Quarantine entries below confidence threshold.
        
        Sets confidence to 0 so they are excluded from retrieval and decay.
        
        Args:
            user_id: User whose knowledge to check
            threshold: Minimum confidence to keep active
            
        Returns:
            Number of entries quarantined
        """
        from api.models import KnowledgeEntry
        
        count = self.db.query(KnowledgeEntry).filter(
            KnowledgeEntry.user_id == user_id,
            KnowledgeEntry.confidence < threshold,
            KnowledgeEntry.confidence > 0,
        ).update(
            {KnowledgeEntry.confidence: 0, KnowledgeEntry.updated_at: datetime.now()},
            synchronize_session=False,
        )
        
        if count:
            self.db.commit()
            logger.info("Quarantined %d low-confidence entries for user %s", count, user_id)
        
        return count
