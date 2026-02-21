"""Per-user BYOD connection pool with lazy initialization."""

from __future__ import annotations

import threading
from urllib.parse import quote_plus

from sqlalchemy import create_engine, text
from sqlalchemy.engine import Engine
from sqlalchemy.orm import Session


class _ConnInfo:
    """Minimal interface for connection info passed to pool methods."""
    dialect: str
    host: str
    port: int
    database: str
    username: str
    password_decrypted: str


class UserDBPool:
    """Manages per-user SQLAlchemy engines for BYOD connections.

    Thread-safe. Engines are created lazily on first access and cached.
    """

    def __init__(
        self,
        pool_size: int = 3,
        max_overflow: int = 2,
        pool_recycle: int = 1800,
    ):
        self._engines: dict[str, Engine] = {}
        self._lock = threading.Lock()
        self._pool_size = pool_size
        self._max_overflow = max_overflow
        self._pool_recycle = pool_recycle

    @staticmethod
    def _build_url(conn: _ConnInfo) -> str:
        """Build SQLAlchemy URL from connection info.

        Both mysql and matrixone use pymysql driver with mysql dialect.
        Password is URL-encoded to handle special characters.
        """
        user = quote_plus(conn.username)
        pw = quote_plus(conn.password_decrypted)
        return (
            f"mysql+pymysql://{user}:{pw}"
            f"@{conn.host}:{conn.port}/{conn.database}?charset=utf8mb4"
        )

    def get_engine(self, user_id: str, conn: _ConnInfo) -> Engine:
        """Get or create an engine for *user_id*."""
        with self._lock:
            if user_id not in self._engines:
                self._engines[user_id] = create_engine(
                    self._build_url(conn),
                    pool_size=self._pool_size,
                    max_overflow=self._max_overflow,
                    pool_recycle=self._pool_recycle,
                    pool_pre_ping=True,
                )
            return self._engines[user_id]

    def get_session(self, user_id: str, conn: _ConnInfo) -> Session:
        """Return a new Session bound to the user's engine."""
        return Session(self.get_engine(user_id, conn))

    def test_connection(self, conn: _ConnInfo) -> bool:
        """Test a BYOD connection without caching the engine."""
        url = self._build_url(conn)
        eng = create_engine(url, pool_size=1, max_overflow=0)
        try:
            with eng.connect() as c:
                c.execute(text("SELECT 1"))
            return True
        except Exception:
            return False
        finally:
            eng.dispose()

    def close_user(self, user_id: str) -> None:
        """Dispose the engine for *user_id*."""
        with self._lock:
            engine = self._engines.pop(user_id, None)
        if engine is not None:
            engine.dispose()

    def close_all(self) -> None:
        """Dispose all engines."""
        with self._lock:
            engines = list(self._engines.values())
            self._engines.clear()
        for eng in engines:
            eng.dispose()
