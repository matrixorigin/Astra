"""Database schema initialization for enhanced hallucination firewall.

Auto-creates tables on first use - no migration needed.
"""

from core.logging_config import get_logger

logger = get_logger(__name__)


def init_hallucination_tables(db):
    """Initialize hallucination firewall tables.

    Creates tables if they don't exist. Safe to call multiple times.

    Args:
        db: Database connection
    """
    try:
        # Create hallucination_checks table
        db.execute(
            """
            CREATE TABLE IF NOT EXISTS hallucination_checks (
                check_id VARCHAR(255) PRIMARY KEY,
                session_id VARCHAR(255) NOT NULL,
                event_id VARCHAR(255) NOT NULL,
                context_capture_id VARCHAR(255) NOT NULL,
                claims_total INT NOT NULL DEFAULT 0,
                claims_verified INT NOT NULL DEFAULT 0,
                claims_contradicted INT NOT NULL DEFAULT 0,
                confidence_score FLOAT NOT NULL DEFAULT 0.0,
                safe_to_deliver BOOLEAN NOT NULL DEFAULT TRUE,
                evidence_count INT NOT NULL DEFAULT 0,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                INDEX idx_session (session_id),
                INDEX idx_event (event_id),
                INDEX idx_context_capture (context_capture_id),
                INDEX idx_confidence (confidence_score),
                INDEX idx_created (created_at)
            )
            """
        )

        # Create claim_evidence table
        db.execute(
            """
            CREATE TABLE IF NOT EXISTS claim_evidence (
                evidence_id BIGINT AUTO_INCREMENT PRIMARY KEY,
                check_id VARCHAR(255) NOT NULL,
                claim_type VARCHAR(50) NOT NULL,
                claim_value TEXT NOT NULL,
                source_type VARCHAR(50) NOT NULL,
                source_id VARCHAR(255) NOT NULL,
                content TEXT NOT NULL,
                location VARCHAR(500) NOT NULL,
                confidence FLOAT NOT NULL DEFAULT 0.0,
                created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                INDEX idx_check (check_id),
                INDEX idx_claim_type (claim_type),
                INDEX idx_source (source_type, source_id),
                INDEX idx_confidence (confidence)
            )
            """
        )

        db.commit()
        logger.info("Hallucination firewall tables initialized")

    except Exception as e:
        logger.error(f"Failed to initialize hallucination tables: {e}")
        db.rollback()
        raise
