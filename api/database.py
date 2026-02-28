"""Database connection and session management with SQLAlchemy."""

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
        cols = {c["name"] for c in inspector.get_columns("skills_registry", schema=engine.url.database)}
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

    # Create IVF-flat vector indexes for L2_DISTANCE queries.
    # SQLAlchemy Index doesn't support USING ivfflat syntax, so we create via raw DDL.
    # MatrixOne doesn't support IF NOT EXISTS for CREATE INDEX, so check first.
    #
    # lists parameter: controls IVF-flat partitioning. Should be ~sqrt(N) for optimal
    # recall/speed tradeoff. We use lists=10 as a safe default for early-stage data
    # (works well up to ~10K rows). Revisit when any table exceeds 10K embeddings.
    _VECTOR_INDEXES = [
        ("mem_memories", "idx_memory_embedding", "mem_memories(embedding)"),
        ("ctx_ctx_event_embeddings", "idx_event_emb_vec", "ctx_ctx_event_embeddings(embedding)"),
        ("sk_knowledge_entries", "idx_knowledge_emb_vec", "sk_knowledge_entries(embedding)"),
        ("skill_selection_learningss", "idx_learning_emb_vec", "skill_selection_learningss(query_embedding)"),
    ]
    for tbl, idx_name, idx_target in _VECTOR_INDEXES:
        if tbl in existing or tbl in {t.name for t in tables_to_create}:
            try:
                with engine.begin() as conn:
                    idx_rows = conn.execute(text(f"SHOW INDEX FROM {tbl}")).fetchall()
                    has_vec_idx = any(idx_name in str(r) for r in idx_rows)
                    if not has_vec_idx:
                        conn.execute(text(
                            f"CREATE INDEX {idx_name} "
                            f"USING ivfflat ON {idx_target} lists=10 op_type 'vector_l2_ops'"
                        ))
            except Exception as e:
                logger.warning("Migration: failed to create vector index %s: %s", idx_name, e)


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
