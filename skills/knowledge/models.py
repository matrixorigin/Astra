"""Knowledge skill tables — platform DB with sk_knowledge_ prefix."""

from sqlalchemy import Column, DateTime, Float, Integer, String, Text, UniqueConstraint
from sqlalchemy.sql import func

from api.base import Base
from matrixone.sqlalchemy_ext import VectorType, VectorPrecision


class SkKnowledgeEntry(Base):
    """Semantic memory: extracted knowledge that persists across sessions."""
    __tablename__ = "sk_knowledge_entries"
    entry_id = Column(String(64), primary_key=True)
    user_id = Column(String(64), nullable=False, index=True)
    agent_id = Column(String(64))

    # What
    category = Column(String(50), nullable=False, index=True)
    key_name = Column(String(255), nullable=False, index=True)
    value = Column(Text, nullable=False)

    # Provenance: see SkKnowledgeEntrySource
    extraction_method = Column(String(50))

    # Trust & Lifecycle
    trust_tier = Column(String(10), default="T3")
    confidence = Column(Float, default=1.0)
    initial_confidence = Column(Float, default=1.0)
    last_validated_at = Column(DateTime, default=func.now())
    last_accessed_at = Column(DateTime)
    access_count = Column(Integer, default=0)

    # Versioning
    version = Column(Integer, default=1)
    superseded_by = Column(String(64))

    # Vector search
    embedding = Column(VectorType(1536, VectorPrecision.F32))

    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())


class SkKnowledgeEntrySource(Base):
    """Provenance: which events directly produced each knowledge entry."""
    __tablename__ = "sk_knowledge_entry_sources"
    entry_id = Column(String(64), primary_key=True)
    event_id = Column(String(64), primary_key=True, index=True)


class SkKnowledgeRelation(Base):
    """Entity-relationship layer over sk_knowledge_entries (knowledge graph edges)."""
    __tablename__ = "sk_knowledge_relations"
    __table_args__ = (
        UniqueConstraint("subject_id", "predicate", "object_id", name="uq_sk_knowledge_spo"),
    )

    relation_id = Column(String(36), primary_key=True)
    subject_id = Column(String(64), nullable=False)
    predicate = Column(String(100), nullable=False)
    object_id = Column(String(64), nullable=False, index=True)
    weight = Column(Float, default=1.0)
    source = Column(String(50))
    created_at = Column(DateTime, default=func.now(), nullable=False)
