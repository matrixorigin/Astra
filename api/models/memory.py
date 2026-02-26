"""Memory SQLAlchemy model — replaces Observation."""

from matrixone import VectorPrecision, VectorType
from matrixone.sqlalchemy_ext import FulltextIndex, FulltextParserType
from sqlalchemy import (
    Column, DateTime, Float, Index, Integer, JSON, SmallInteger, String, Text,
)
from sqlalchemy.sql import func

from api.base import Base


class MemoryRecord(Base):
    """Typed, versioned memory with vector embedding and fulltext index."""

    __tablename__ = "memories"
    __table_args__ = (
        FulltextIndex("ft_memory_content", ["content"], parser=FulltextParserType.NGRAM),
        Index("idx_memory_user_type_active", "user_id", "memory_type", "is_active"),
        Index("idx_memory_user_active", "user_id", "is_active"),
        Index("idx_memory_observed_at", "observed_at"),
    )

    memory_id = Column(String(64), primary_key=True)
    user_id = Column(String(64), nullable=False)
    memory_type = Column(String(20), nullable=False)  # profile/episodic/semantic/procedural/working
    content = Column(Text, nullable=False)
    confidence = Column(Float, default=0.75, nullable=False)
    embedding = Column(VectorType(1536, VectorPrecision.F32))
    source_event_ids = Column(JSON, nullable=False, default=list)
    superseded_by = Column(String(64), nullable=True)
    is_active = Column(SmallInteger, server_default="1", nullable=False)
    observed_at = Column(DateTime, nullable=False)  # PITR time anchor
    created_at = Column(DateTime, default=func.now(), nullable=False)
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
