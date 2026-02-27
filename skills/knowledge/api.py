"""Knowledge skill API — typed interface for knowledge data access.

Consolidates knowledge entry CRUD, graph relations, and extraction logic.
"""

from __future__ import annotations

import json
import re
from datetime import datetime
from typing import Any

from sqlalchemy import text
from sqlalchemy.orm import Session
from uuid_utils import uuid7

from core.logging_config import get_logger

logger = get_logger(__name__)


# ── Constants ─────────────────────────────────────────────────────────────────

KNOWLEDGE_EXTRACTION_PROMPT = """\
You extract structured knowledge from conversations. Output a JSON array ONLY.

Each item must have:
- "category": one of "user_preference", "codebase_pattern", "domain_fact", "tool_behavior", "entity"
- "key_name": short unique key (e.g. "preferred_language", "auth.pattern")
- "value": the extracted fact

Only extract clear, factual statements. Skip vague or uncertain information.
"""


# ── Helpers ───────────────────────────────────────────────────────────────────

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


# ── Access tracking ───────────────────────────────────────────────────────────

def update_access_tracking(db: Session, entry_ids: list[str]) -> None:
    """Bump access_count and last_accessed_at for retrieved entries.

    Single UPDATE — no N+1.  Safe to call with empty list.
    """
    if not entry_ids:
        return
    try:
        db.execute(
            text(
                "UPDATE sk_knowledge_entries "
                "SET access_count = access_count + 1, last_accessed_at = NOW() "
                "WHERE entry_id IN :ids"
            ),
            {"ids": tuple(entry_ids)},
        )
        db.commit()
    except Exception as e:
        logger.warning("Access tracking update failed: %s", e)
        try:
            db.rollback()
        except Exception:
            pass


# ── Knowledge Graph (relations) ───────────────────────────────────────────────

def add_relation(
    db: Session,
    subject_id: str,
    predicate: str,
    object_id: str,
    *,
    weight: float = 1.0,
    source: str = "extraction",
) -> str | None:
    """Add a directed edge between two knowledge entries.

    Upserts: if (subject, predicate, object) already exists, updates weight/source.
    Returns relation_id on success, None on failure.

    Note: Uses SELECT + INSERT/UPDATE instead of ON DUPLICATE KEY UPDATE because
    MatrixOne accepts the syntax but does not execute the UPDATE branch (bug).
    """
    try:
        row = db.execute(
            text(
                "SELECT relation_id FROM sk_knowledge_relations "
                "WHERE subject_id = :sid AND predicate = :pred AND object_id = :oid"
            ),
            {"sid": subject_id, "pred": predicate, "oid": object_id},
        ).fetchone()

        if row:
            db.execute(
                text(
                    "UPDATE sk_knowledge_relations SET weight = :w, source = :src "
                    "WHERE relation_id = :rid"
                ),
                {"w": weight, "src": source, "rid": row.relation_id},
            )
            db.commit()
            return row.relation_id

        rid = str(uuid7())
        db.execute(
            text("""
                INSERT INTO sk_knowledge_relations
                (relation_id, subject_id, predicate, object_id, weight, source, created_at)
                VALUES (:rid, :sid, :pred, :oid, :w, :src, NOW())
            """),
            {"rid": rid, "sid": subject_id, "pred": predicate, "oid": object_id, "w": weight, "src": source},
        )
        db.commit()
        return rid
    except Exception as e:
        logger.warning("Failed to add relation %s -[%s]-> %s: %s", subject_id, predicate, object_id, e)
        db.rollback()
        return None


def get_neighbors(
    db: Session,
    entry_id: str,
    *,
    predicates: list[str] | None = None,
    direction: str = "both",
    limit: int = 20,
) -> list[dict]:
    """Get 1-hop neighbors of a knowledge entry.

    Args:
        entry_id: Source knowledge entry ID.
        predicates: Filter by relationship types (None = all).
        direction: "outgoing", "incoming", or "both".
        limit: Max neighbors to return.

    Returns:
        List of dicts with neighbor_id, predicate, weight, direction.
    """
    clauses = []
    params: dict = {"eid": entry_id, "limit": limit}

    pred_filter = ""
    if predicates:
        pred_ph = ", ".join(f":p{i}" for i in range(len(predicates)))
        pred_filter = f"AND predicate IN ({pred_ph})"
        for i, p in enumerate(predicates):
            params[f"p{i}"] = p

    if direction in ("outgoing", "both"):
        clauses.append(f"""
            SELECT object_id AS neighbor_id, predicate, weight, 'outgoing' AS dir
            FROM sk_knowledge_relations WHERE subject_id = :eid {pred_filter}
        """)
    if direction in ("incoming", "both"):
        clauses.append(f"""
            SELECT subject_id AS neighbor_id, predicate, weight, 'incoming' AS dir
            FROM sk_knowledge_relations WHERE object_id = :eid {pred_filter}
        """)

    sql = " UNION ALL ".join(clauses) + " ORDER BY weight DESC LIMIT :limit"

    try:
        rows = db.execute(text(sql), params).fetchall()
        return [
            {"neighbor_id": r.neighbor_id, "predicate": r.predicate, "weight": float(r.weight), "direction": r.dir}
            for r in rows
        ]
    except Exception as e:
        logger.warning("get_neighbors failed for %s: %s", entry_id, e)
        return []


def expand_with_graph(
    db: Session,
    entry_ids: list[str],
    *,
    limit_per_entry: int = 3,
) -> list[str]:
    """1-hop graph expansion: given seed entry IDs, find related entries.

    Returns additional entry IDs (not including seeds) sorted by total weight.
    """
    if not entry_ids:
        return []

    placeholders = ", ".join(f":e{i}" for i in range(len(entry_ids)))
    params = {f"e{i}": eid for i, eid in enumerate(entry_ids)}
    params["lim"] = limit_per_entry * len(entry_ids)

    sql = text(f"""
        SELECT neighbor_id, SUM(weight) AS total_weight FROM (
            SELECT object_id AS neighbor_id, weight
            FROM sk_knowledge_relations WHERE subject_id IN ({placeholders})
            UNION ALL
            SELECT subject_id AS neighbor_id, weight
            FROM sk_knowledge_relations WHERE object_id IN ({placeholders})
        ) t
        WHERE neighbor_id NOT IN ({placeholders})
        GROUP BY neighbor_id
        ORDER BY total_weight DESC
        LIMIT :lim
    """)

    try:
        rows = db.execute(sql, params).fetchall()
        return [r.neighbor_id for r in rows]
    except Exception as e:
        logger.warning("expand_with_graph failed: %s", e)
        return []


# ── Knowledge Extractor ──────────────────────────────────────────────────────

class KnowledgeExtractor:
    """Extract semantic knowledge from conversation events.

    Post-chain hook that analyzes completed causal chains and extracts
    structured knowledge (user preferences, codebase patterns, domain facts).
    """

    PREFERENCE_PATTERNS = re.compile(
        r'\b(i prefer|i like|i want|i use|i always|i never)\b',
        re.IGNORECASE,
    )
    PATTERN_PATTERNS = re.compile(
        r'\b(pattern|architecture|uses|implements|follows)\b',
        re.IGNORECASE,
    )

    def __init__(self, db: Session, llm_client=None, event_logger=None):
        self.db = db
        self.llm = llm_client
        self.event_logger = event_logger

    def extract_from_chain(self, causal_chain_id: str, user_id: str) -> list[dict[str, Any]]:
        """Extract knowledge from completed causal chain."""
        from api.models import Event

        events = self.db.query(Event).filter(
            Event.causal_chain_id == causal_chain_id,
            Event.user_id == user_id,
        ).order_by(Event.created_at).all()

        if not events:
            return []

        if self.llm:
            extracted = self._extract_via_llm(events, user_id)
        else:
            extracted = self._extract_via_regex(events, user_id)

        stored = self._batch_store_knowledge(extracted)

        if self.event_logger and stored:
            try:
                self.event_logger.create_stream_event(
                    user_id=user_id,
                    session_id="system",
                    event_type="knowledge_extracted",
                    content=json.dumps({
                        "causal_chain_id": causal_chain_id,
                        "entries": stored,
                    }),
                    metadata={
                        "causal_chain_id": causal_chain_id,
                        "count": len(stored),
                    },
                )
            except Exception as e:
                logger.warning("Failed to log extraction event: %s", e)

        logger.info("Extracted %d knowledge entries from chain %s", len(stored), causal_chain_id)
        return stored

    def _extract_via_llm(self, events, user_id: str) -> list[dict[str, Any]]:
        from core.memory.typed_observer import _parse_json_array
        from core.memory.types import trust_tier_defaults

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
        content = event.content
        if "typescript" in content.lower():
            from core.memory.types import trust_tier_defaults
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
        content = event.content.lower()
        if "dependency injection" in content:
            from core.memory.types import trust_tier_defaults
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
        from api.models import KnowledgeEntry, KnowledgeEntrySource

        if not entries:
            return []

        stored = []
        entries_by_key = {}
        for entry in entries:
            key = (entry["user_id"], entry["category"], entry["key_name"])
            entries_by_key[key] = entry

        keys_to_check = [(e["user_id"], e["category"], e["key_name"]) for e in entries_by_key.values()]

        existing_entries = {}
        if keys_to_check:
            from sqlalchemy import or_, and_
            conditions = [
                and_(
                    KnowledgeEntry.user_id == user_id,
                    KnowledgeEntry.category == category,
                    KnowledgeEntry.key_name == key_name,
                )
                for user_id, category, key_name in keys_to_check
            ]
            existing = self.db.query(KnowledgeEntry).filter(or_(*conditions)).all()
            for e in existing:
                existing_entries[(e.user_id, e.category, e.key_name)] = e

        for key, entry in entries_by_key.items():
            source_ids = entry.get("source_event_ids", [])
            if key in existing_entries:
                existing = existing_entries[key]
                now = datetime.now()
                if _normalize_value(existing.value) == _normalize_value(entry["value"]):
                    existing.confidence = min(1.0, existing.confidence + 0.1)
                    existing.version += 1
                    existing.last_validated_at = now
                    existing.updated_at = now
                    for eid in source_ids:
                        self.db.execute(text(
                            "INSERT IGNORE INTO sk_knowledge_entry_sources (entry_id, event_id) "
                            "VALUES (:eid, :evid)"
                        ), {"eid": existing.entry_id, "evid": eid})
                    stored.append({
                        "entry_id": existing.entry_id,
                        "action": "updated",
                        "confidence": existing.confidence,
                    })
                else:
                    logger.warning(
                        "Knowledge contradiction: %s = %r vs %r",
                        entry["key_name"], existing.value, entry["value"],
                    )
                    existing.confidence = max(0.0, existing.confidence - 0.3)
                    existing.updated_at = now
                    entry_id = str(uuid7())
                    knowledge = KnowledgeEntry(
                        entry_id=entry_id,
                        user_id=entry["user_id"],
                        category=entry["category"],
                        key_name=entry["key_name"],
                        value=entry["value"],
                        extraction_method=entry.get("extraction_method", "observation"),
                        trust_tier=entry["trust_tier"],
                        confidence=entry["confidence"],
                        initial_confidence=entry["confidence"],
                    )
                    existing.superseded_by = entry_id
                    self.db.add(knowledge)
                    for eid in source_ids:
                        self.db.add(KnowledgeEntrySource(entry_id=entry_id, event_id=eid))
                    stored.append({
                        "entry_id": entry_id,
                        "action": "contradiction",
                        "confidence": entry["confidence"],
                    })
            else:
                entry_id = str(uuid7())
                knowledge = KnowledgeEntry(
                    entry_id=entry_id,
                    user_id=entry["user_id"],
                    category=entry["category"],
                    key_name=entry["key_name"],
                    value=entry["value"],
                    extraction_method=entry["extraction_method"],
                    trust_tier=entry["trust_tier"],
                    confidence=entry["confidence"],
                    initial_confidence=entry["confidence"],
                )
                self.db.add(knowledge)
                for eid in source_ids:
                    self.db.add(KnowledgeEntrySource(entry_id=entry_id, event_id=eid))
                stored.append({
                    "entry_id": entry_id,
                    "action": "created",
                    "confidence": entry["confidence"],
                })
                logger.info("Created knowledge entry: %s (confidence=%s)", entry["key_name"], entry["confidence"])

        self.db.commit()
        return stored

    def decay_confidence(self, user_id: str, half_life_days: int = 60) -> int:
        """Apply confidence decay to knowledge entries."""
        from api.models import KnowledgeEntry
        from core.memory.types import TRUST_TIER_HALF_LIVES, TrustTier

        entries = self.db.query(KnowledgeEntry).filter(
            KnowledgeEntry.user_id == user_id,
            KnowledgeEntry.confidence > 0.3,
        ).all()

        count = 0
        now = datetime.now()
        for entry in entries:
            anchor = entry.last_validated_at or entry.created_at
            if anchor is None:
                continue
            hl = TRUST_TIER_HALF_LIVES.get(TrustTier(entry.trust_tier), half_life_days) if entry.trust_tier else half_life_days
            days_since = (now - anchor).days
            new_conf = entry.initial_confidence * (0.5 ** (days_since / hl))
            if new_conf != entry.confidence:
                entry.confidence = new_conf
                entry.updated_at = now
                count += 1

        self.db.commit()
        logger.info("Applied confidence decay to %d entries for user %s", count, user_id)
        return count

    def quarantine_low_confidence(self, user_id: str, threshold: float = 0.3) -> int:
        """Quarantine entries below confidence threshold."""
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
