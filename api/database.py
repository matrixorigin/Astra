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
)

# Create engine
engine = create_engine(
    DATABASE_URL,
    pool_pre_ping=True,  # Verify connections before using
    pool_recycle=3600,   # Recycle connections after 1 hour
    echo=False,          # Set to True for SQL logging
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
    """Initialize database - create all tables."""
    from api.models import Base
    Base.metadata.create_all(bind=engine)
