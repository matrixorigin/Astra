"""Database connection management.

Provides connection pooling and context managers for MatrixOne database operations.
"""

from collections.abc import Generator
from contextlib import contextmanager

import pymysql
from pymysql.connections import Connection
from pymysql.cursors import DictCursor

from config.settings import get_settings


class Database:
    """Database connection manager with connection pooling."""

    def __init__(self) -> None:
        """Initialize database manager."""
        self.settings = get_settings()
        self._connection: Connection | None = None

    def _create_connection(self) -> Connection:
        """Create a new database connection.

        Returns:
            Connection: PyMySQL connection object
        """
        return pymysql.connect(
            host=self.settings.matrixone_host,
            port=self.settings.matrixone_port,
            user=self.settings.matrixone_user,
            password=self.settings.matrixone_password,
            database=self.settings.matrixone_database,
            charset="utf8mb4",
            cursorclass=DictCursor,
            autocommit=False,
        )

    @contextmanager
    def get_connection(self) -> Generator[Connection, None, None]:
        """Get a database connection with automatic cleanup.

        Yields:
            Connection: Database connection

        Example:
            >>> db = Database()
            >>> with db.get_connection() as conn:
            ...     with conn.cursor() as cursor:
            ...         cursor.execute("SELECT 1")
        """
        conn = self._create_connection()
        try:
            yield conn
            conn.commit()
        except Exception:
            conn.rollback()
            raise
        finally:
            conn.close()

    @contextmanager
    def get_cursor(self) -> Generator[DictCursor, None, None]:
        """Get a database cursor with automatic connection management.

        Yields:
            DictCursor: Database cursor that returns results as dictionaries

        Example:
            >>> db = Database()
            >>> with db.get_cursor() as cursor:
            ...     cursor.execute("SELECT * FROM conversation_events LIMIT 1")
            ...     result = cursor.fetchone()
        """
        with self.get_connection() as conn:
            cursor: DictCursor = conn.cursor()  # type: ignore[assignment]
            try:
                yield cursor
            finally:
                cursor.close()

    def execute(self, query: str, params: tuple | None = None) -> int:
        """Execute a query and return affected rows.

        Args:
            query: SQL query
            params: Query parameters

        Returns:
            int: Number of affected rows
        """
        with self.get_cursor() as cursor:
            result = cursor.execute(query, params)
            return int(result)

    def fetchone(self, query: str, params: tuple | None = None) -> dict | None:
        """Execute a query and fetch one result.

        Args:
            query: SQL query
            params: Query parameters

        Returns:
            Optional[dict]: Query result as dictionary, or None
        """
        with self.get_cursor() as cursor:
            cursor.execute(query, params)
            result = cursor.fetchone()
            return dict(result) if result else None

    def fetchall(self, query: str, params: tuple | None = None) -> list[dict]:
        """Execute a query and fetch all results.

        Args:
            query: SQL query
            params: Query parameters

        Returns:
            list[dict]: Query results as list of dictionaries
        """
        with self.get_cursor() as cursor:
            cursor.execute(query, params)
            results = cursor.fetchall()
            return [dict(row) for row in results]
