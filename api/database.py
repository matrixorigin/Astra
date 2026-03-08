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
    """Initialize database - create all tables if not exist."""
    from sqlalchemy import text

    from api.models import Base

    _import_skill_models()
    Base.metadata.create_all(bind=engine, checkfirst=True)

    # Sync seed quirks (e.g. fixed_temperature) into existing model rows.
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
        logger.warning("Seed quirks sync failed: %s", e)

    # Create IVF-flat vector indexes.
    from matrixone import VectorIndex

    for tbl, idx_name, col_name in [
        ("mem_memories", "idx_memory_embedding", "embedding"),
        ("ctx_ctx_event_embeddings", "idx_event_emb_vec", "embedding"),
        ("sk_knowledge_entries", "idx_knowledge_emb_vec", "embedding"),
        ("skill_selection_learningss", "idx_learning_emb_vec", "query_embedding"),
    ]:
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
            logger.debug("Vector index %s skipped: %s", idx_name, e)


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
