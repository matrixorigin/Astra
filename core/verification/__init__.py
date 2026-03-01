"""Verification module for LLM response validation.

Enhanced with:
- LLM-based claim extraction
- Structured verification with evidence backlinks
- Semantic confidence scoring
"""

from core.verification.claim_extractor import ClaimExtractor
from core.verification.firewall import HallucinationFirewall
from core.verification.llm_claim_extractor import Claim, LLMClaimExtractor
from core.verification.structured_verifier import (
    Evidence,
    StructuredVerifier,
    VerificationResult,
)
from core.verification.tool_quality import (
    QualityAssessment,
    annotate_tool_result,
    assess_tool_result,
    assess_with_schema,
    load_quality_schema,
    set_schema_loader,
    invalidate_schema_cache,
)

__all__ = [
    "HallucinationFirewall",
    "ClaimExtractor",
    "LLMClaimExtractor",
    "StructuredVerifier",
    "Claim",
    "Evidence",
    "VerificationResult",
    "QualityAssessment",
    "assess_tool_result",
    "annotate_tool_result",
]
