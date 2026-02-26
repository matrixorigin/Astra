"""Database schema for hallucination firewall — DEPRECATED.

Tables are now defined as ORM models in api/models.py (HallucinationCheck, ClaimEvidence)
and created automatically by init_db().
"""


def init_hallucination_tables(db):
    """No-op: tables are now created by ORM via init_db()."""
    pass
