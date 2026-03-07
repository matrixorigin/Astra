"""Database connection and session management with SQLAlchemy."""

import json
import logging
from contextlib import contextmanager
from decimal import Decimal

from sqlalchemy import text
from sqlalchemy.orm import Session, sessionmaker

from config.settings import get_settings

logger = logging.getLogger(__name__)
settings = get_settings()


def decimal_default(obj):
    """JSON encoder for Decimal objects."""
    if isinstance(obj, Decimal):
        return float(obj)
    raise TypeError


def decimal_decoder(dct):
    """JSON decoder that converts Decimal to float."""
    return {k: float(v) if isinstance(v, Decimal) else v for k, v in dct.items()}


# Database URL (kept for reference; engine uses MatrixOne client below)
DATABASE_URL = (
    f"mysql+pymysql://{settings.matrixone_user}:{settings.matrixone_password}"
    f"@{settings.matrixone_host}:{settings.matrixone_port}/{settings.matrixone_database}"
    "?charset=utf8mb4"
)

from matrixone import Client as _MoClient  # noqa: E402
from matrixone.sqlalchemy_ext import FulltextIndex as _FulltextIndex  # noqa: E402
from sqlalchemy.dialects.mysql.base import MySQLDDLCompiler as _MySQLDDLCompiler  # noqa: E402

# Workaround: MatrixOne dialect has default_schema_name=None, breaking has_table()
# and visit_create_index() for FulltextIndex. Patch both.

_orig_visit_create_index = _MySQLDDLCompiler.visit_create_index

def _visit_create_index(self, create, **kw):
    idx = create.element
    if isinstance(idx, _FulltextIndex):
        cols = ", ".join(col.name for col in idx.columns)
        sql = f"CREATE FULLTEXT INDEX {idx.name} ON {idx.table.name} ({cols})"
        if getattr(idx, "parser", None):
            sql += f" WITH PARSER {idx.parser}"
        return sql
    return _orig_visit_create_index(self, create, **kw)

_MySQLDDLCompiler.visit_create_index = _visit_create_index

# Ensure database exists before connecting
_bootstrap = _MoClient(
    host=settings.matrixone_host,
    port=settings.matrixone_port,
    user=settings.matrixone_user,
    password=settings.matrixone_password,
    database="mo_catalog",
    sql_log_mode="off",
)
with _bootstrap._engine.connect() as _c:
    _c.execute(text(f"CREATE DATABASE IF NOT EXISTS `{settings.matrixone_database}`"))
    _c.execute(text("COMMIT"))
_bootstrap._engine.dispose()

# Use MatrixOne client's engine (supports vecf32/vecf64 and FulltextIndex DDL)
_mo_client = _MoClient(
    host=settings.matrixone_host,
    port=settings.matrixone_port,
    user=settings.matrixone_user,
    password=settings.matrixone_password,
    database=settings.matrixone_database,
    sql_log_mode="off",
)
engine = _mo_client._engine

# Session factory
SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=engine)


def get_db_session() -> Session:
    """Get database session for dependency injection.
    
    Yields:
        Session: SQLAlchemy session
    """
    db = SessionLocal()
    try:
        yield db
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


@contextmanager
def get_db_context():
    """Get database session as context manager.
    
    Yields:
        Session: SQLAlchemy session
    """
    db = SessionLocal()
    try:
        yield db
        db.commit()
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


def _get_column_names(inspector, table_name: str, schema: str | None) -> set[str]:
    """Get column names, suppressing SAWarning from FULLTEXT WITH PARSER ngram.

    MatrixOne's ``FULLTEXT ... WITH PARSER ngram`` DDL triggers an SAWarning
    in SQLAlchemy's MySQL reflection parser (``Unknown schema content``).
    This is a known gap in the MatrixOne SDK's dialect — its ``get_columns``
    delegates to ``super().get_columns()`` which parses ``SHOW CREATE TABLE``
    and chokes on the ngram parser clause.  Harmless — we only need column names.
    """
    import warnings
    from sqlalchemy.exc import SAWarning
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", SAWarning)
        return {c["name"] for c in inspector.get_columns(table_name, schema=schema)}


def init_db():
    """Initialize database - create tables and indexes if not exist."""
    from sqlalchemy import inspect, text

    from api.models import Base

    # Auto-discover skill models (skills/*/models.py) so their tables are in Base.metadata
    _import_skill_models()

    inspector = inspect(engine)
    existing = set(inspector.get_table_names(schema=engine.url.database))
    tables_to_create = [t for t in Base.metadata.sorted_tables if t.name not in existing]
    if tables_to_create:
        Base.metadata.create_all(bind=engine, tables=tables_to_create, checkfirst=True)

    # Migrate: add columns merged from skill_definitions into skills_registry
    if "skills_registry" in existing:
        cols = _get_column_names(inspector, "skills_registry", engine.url.database)
        with engine.begin() as conn:
            for col, ddl in [
                ("source", "VARCHAR(20) DEFAULT 'builtin'"),
                ("manifest", "JSON"),
                ("is_public", "SMALLINT DEFAULT 0"),
                ("created_by", "VARCHAR(36)"),
            ]:
                if col not in cols:
                    try:
                        conn.execute(text(f"ALTER TABLE skills_registry ADD COLUMN {col} {ddl}"))
                    except Exception as e:
                        logger.warning("Migration: failed to add column %s: %s", col, e)

    # Migrate: add quirks column to infra_llm_models + sync seed quirks
    if "infra_llm_models" in existing:
        cols = _get_column_names(inspector, "infra_llm_models", engine.url.database)
        if "quirks" not in cols:
            try:
                with engine.begin() as conn:
                    conn.execute(text("ALTER TABLE infra_llm_models ADD COLUMN quirks JSON NULL COMMENT 'ModelQuirks — model-specific behavioral overrides'"))
            except Exception as e:
                logger.warning("Migration: failed to add infra_llm_models.quirks: %s", e)

        # Backfill seed quirks for models that have NULL or empty quirks in DB.
        # Only overwrites when the DB value is missing — admin-customized quirks
        # are preserved.  This ensures new seed quirk fields (e.g. fixed_temperature
        # added after initial registration) propagate to existing installations.
        # NOTE: MatrixOne panics on `json_col = '{}'` (direct JSON-to-string comparison).
        # Use CAST(quirks AS CHAR) for safe string comparison.
        try:
            from core.llm.seed_models import SEED_MODELS
            with engine.begin() as conn:
                for sm in SEED_MODELS:
                    if sm.get("quirks"):
                        conn.execute(text(
                            "UPDATE infra_llm_models SET quirks = :q "
                            "WHERE model_name = :m AND (quirks IS NULL OR CAST(quirks AS CHAR) = '{}')"
                        ), {"q": json.dumps(sm["quirks"]), "m": sm["model_name"]})
        except Exception as e:
            logger.warning("Migration: failed to sync seed quirks: %s", e)

    # Migrate: add reasoning_content column to agent_events
    if "agent_events" in existing:
        cols = _get_column_names(inspector, "agent_events", engine.url.database)
        if "reasoning_content" not in cols:
            try:
                with engine.begin() as conn:
                    conn.execute(text("ALTER TABLE agent_events ADD COLUMN reasoning_content TEXT NULL COMMENT 'thinking-model chain-of-thought (e.g. kimi-k2.5)'"))
            except Exception as e:
                logger.warning("Migration: failed to add agent_events.reasoning_content: %s", e)

    # Migrate: upgrade DATETIME(0) → DATETIME(6) on existing tables.
    # Earlier code used SQLAlchemy's generic DateTime(timezone=6) which silently
    # dropped fractional-second precision in DDL.  Models now use DateTime6
    # (MySQL DATETIME(fsp=6)) so new tables are correct, but existing tables
    # need an ALTER.  The loop is idempotent — MODIFY to the same type is a no-op.
    _datetime6_columns = [
        ("agent_events", "created_at", "NOT NULL"),
        ("agent_sessions", "created_at", "NOT NULL"),
        ("agent_sessions", "updated_at", "NOT NULL"),
        ("agent_sessions", "ended_at", ""),
        ("agent_sessions", "last_active_at", "NOT NULL"),
        ("ctx_snapshots", "created_at", "NOT NULL"),
        ("skill_selection_events", "created_at", ""),
    ]
    for tbl, col, extra in _datetime6_columns:
        if tbl in existing:
            try:
                with engine.begin() as conn:
                    conn.execute(text(f"ALTER TABLE {tbl} MODIFY COLUMN {col} DATETIME(6) {extra}"))
            except Exception as e:
                logger.debug("DATETIME(6) migration %s.%s skipped: %s", tbl, col, e)

    # Create IVF-flat vector indexes for L2_DISTANCE queries.
    # MatrixOne SDK handles SET experimental_ivf_index=1 and SQL generation.
    # lists=10 is safe default for early-stage data (up to ~10K rows).
    from matrixone import VectorIndex

    vector_indexes = [
        ("mem_memories", "idx_memory_embedding", "embedding"),
        ("ctx_ctx_event_embeddings", "idx_event_emb_vec", "embedding"),
        ("sk_knowledge_entries", "idx_knowledge_emb_vec", "embedding"),
        ("skill_selection_learningss", "idx_learning_emb_vec", "query_embedding"),
    ]
    for tbl, idx_name, col_name in vector_indexes:
        if tbl in existing or tbl in {t.name for t in tables_to_create}:
            try:
                with engine.begin() as conn:
                    idx_rows = conn.execute(text(f"SHOW INDEX FROM {tbl}")).fetchall()
                    if not any(idx_name in str(r) for r in idx_rows):
                        VectorIndex.create_index(
                            engine, table_name=tbl, name=idx_name,
                            column=col_name, index_type="ivfflat",
                            lists=10, op_type="vector_l2_ops",
                        )
            except Exception as e:
                logger.warning("Failed to create vector index %s: %s", idx_name, e)


def _import_skill_models():
    """Import skills/*/models.py so skill tables register with Base.metadata."""
    import importlib
    from pathlib import Path

    skills_dir = Path(__file__).parent.parent / "skills"
    if not skills_dir.is_dir():
        return
    for skill_dir in sorted(skills_dir.iterdir()):
        if skill_dir.is_dir() and (skill_dir / "models.py").exists():
            importlib.import_module(f"skills.{skill_dir.name}.models")
