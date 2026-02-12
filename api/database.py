"""Database connection and session management with SQLAlchemy."""

from contextlib import contextmanager

from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker, Session

from config.settings import get_settings

settings = get_settings()

# Database URL
DATABASE_URL = (
    f"mysql+pymysql://{settings.matrixone_user}:{settings.matrixone_password}"
    f"@{settings.matrixone_host}:{settings.matrixone_port}/{settings.matrixone_database}"
    "?charset=utf8mb4"
)

# Create engine
engine = create_engine(
    DATABASE_URL,
    pool_pre_ping=True,
    pool_recycle=3600,
    echo=False,
)

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
    """Initialize database - create tables if not exist."""
    from api.models import Base
    from sqlalchemy import inspect
    
    inspector = inspect(engine)
    existing_tables = inspector.get_table_names()
    
    # Check if our tables exist
    required_tables = [
        'users', 'agents', 'refresh_tokens', 'sessions', 'conversation_events',
        'prompt_templates', 'skills_registry', 'context_snapshots', 'decision_audit',
        'event_embeddings', 'repos', 'sandbox_metadata', 'audit_logs'
    ]
    missing = [t for t in required_tables if t not in existing_tables]
    
    if not missing:
        print(f"All required tables exist")
        return
    
    print(f"Creating missing tables: {missing}")
    Base.metadata.create_all(bind=engine)
    print("Tables created successfully")
