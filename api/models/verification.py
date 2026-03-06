"""Hallucination firewall verification models."""

from sqlalchemy import Column, DateTime, Float, Integer, SmallInteger, String, Text
from sqlalchemy.sql import func

from api.base import Base


class HallucinationCheck(Base):
    __tablename__ = "verify_hallucination_checks"
    check_id = Column(String(64), primary_key=True)
    session_id = Column(String(36), nullable=False, index=True)
    event_id = Column(String(36), nullable=False, index=True)
    context_capture_id = Column(String(36), nullable=False, index=True)
    claims_total = Column(Integer, nullable=False, default=0)
    claims_verified = Column(Integer, nullable=False, default=0)
    claims_contradicted = Column(Integer, nullable=False, default=0)
    confidence_score = Column(Float, nullable=False, default=0.0)
    safe_to_deliver = Column(SmallInteger, nullable=False, default=1, server_default="1")
    evidence_count = Column(Integer, nullable=False, default=0)
    created_at = Column(DateTime(6), default=func.now())


class ClaimEvidence(Base):
    __tablename__ = "verify_claim_evidence"
    evidence_id = Column(Integer, primary_key=True, autoincrement=True)
    check_id = Column(String(64), nullable=False, index=True)
    claim_type = Column(String(50), nullable=False)
    claim_value = Column(Text, nullable=False)
    source_type = Column(String(50), nullable=False)
    source_id = Column(String(255), nullable=False)
    content = Column(Text, nullable=False)
    location = Column(String(500), nullable=False)
    confidence = Column(Float, nullable=False, default=0.0)
    created_at = Column(DateTime(6), default=func.now())
