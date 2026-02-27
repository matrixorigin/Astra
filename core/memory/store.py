"""MemoryStore — CRUD + atomic contradiction resolution for memories."""

from __future__ import annotations

import logging
import uuid
from datetime import datetime
from typing import Optional

from sqlalchemy.orm import Session

from api.models.memory import MemoryRecord
from core.db_consumer import DbConsumer, DbFactory
from core.memory.metrics import MemoryMetrics, Timer
from core.memory.types import Memory, MemoryType

logger = logging.getLogger(__name__)


def _to_domain(row: MemoryRecord) -> Memory:
    return Memory(
        memory_id=row.memory_id,
        user_id=row.user_id,
        memory_type=MemoryType(row.memory_type),
        content=row.content,
        initial_confidence=row.initial_confidence,
        embedding=row.embedding,
        source_event_ids=row.source_event_ids or [],
        superseded_by=row.superseded_by,
        is_active=bool(row.is_active),
        session_id=row.session_id,
        observed_at=row.observed_at,
        created_at=row.created_at,
        updated_at=row.updated_at,
    )


class MemoryStore(DbConsumer):
    """CRUD operations on the memories table."""

    def __init__(self, db_factory: DbFactory, metrics: Optional[MemoryMetrics] = None):
        super().__init__(db_factory)
        self._metrics = metrics or MemoryMetrics()

    def create(self, memory: Memory) -> Memory:
        if not memory.memory_id:
            memory.memory_id = uuid.uuid4().hex
        now = datetime.utcnow()
        if not memory.observed_at:
            memory.observed_at = now

        with Timer("store_create", self._metrics):
            with self._db() as db:
                row = MemoryRecord(
                    memory_id=memory.memory_id,
                    user_id=memory.user_id,
                    session_id=memory.session_id,
                    memory_type=memory.memory_type.value,
                    content=memory.content,
                    initial_confidence=memory.initial_confidence,
                    embedding=memory.embedding,
                    source_event_ids=memory.source_event_ids,
                    is_active=1,
                    observed_at=memory.observed_at,
                )
                db.add(row)
                db.commit()
                memory.created_at = row.created_at
        self._metrics.increment("memories_created")
        return memory

    def get(self, memory_id: str) -> Optional[Memory]:
        with Timer("store_get", self._metrics):
            with self._db() as db:
                row = db.query(MemoryRecord).filter_by(memory_id=memory_id).first()
                return _to_domain(row) if row else None

    def list_active(
        self,
        user_id: str,
        memory_type: Optional[MemoryType] = None,
    ) -> list[Memory]:
        with self._db() as db:
            q = db.query(MemoryRecord).filter(
                MemoryRecord.user_id == user_id,
                MemoryRecord.is_active == 1,
            )
            if memory_type:
                q = q.filter(MemoryRecord.memory_type == memory_type.value)
            return [_to_domain(r) for r in q.all()]

    def supersede(self, old_id: str, new_memory: Memory) -> Memory:
        if not new_memory.memory_id:
            new_memory.memory_id = uuid.uuid4().hex
        now = datetime.utcnow()
        if not new_memory.observed_at:
            new_memory.observed_at = now

        with self._db() as db:
            old = db.query(MemoryRecord).filter_by(memory_id=old_id).first()
            if old:
                old.is_active = 0
                old.superseded_by = new_memory.memory_id
                old.updated_at = now

            row = MemoryRecord(
                memory_id=new_memory.memory_id,
                user_id=new_memory.user_id,
                session_id=new_memory.session_id,
                memory_type=new_memory.memory_type.value,
                content=new_memory.content,
                initial_confidence=new_memory.initial_confidence,
                embedding=new_memory.embedding,
                source_event_ids=new_memory.source_event_ids,
                is_active=1,
                observed_at=new_memory.observed_at,
            )
            db.add(row)
            db.commit()
            new_memory.created_at = row.created_at
        return new_memory

    def deactivate(self, memory_id: str) -> bool:
        with self._db() as db:
            row = db.query(MemoryRecord).filter_by(memory_id=memory_id).first()
            if not row:
                return False
            row.is_active = 0
            row.updated_at = datetime.utcnow()
            db.commit()
            return True
