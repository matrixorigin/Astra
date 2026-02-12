"""Database connection with tenant support."""

import os
from contextlib import contextmanager

import pymysql
from dotenv import load_dotenv

from core.logging_config import get_logger

load_dotenv()
logger = get_logger(__name__)


class DatabaseConfig:
    """Database configuration from environment."""

    def __init__(self):
        self.host = os.getenv("DATABASE_HOST", "localhost")
        self.port = int(os.getenv("DATABASE_PORT", "6001"))
        self.user = os.getenv("DATABASE_USER", "dump")
        self.password = os.getenv("DATABASE_PASSWORD", "111")
        self.tenant = os.getenv("DATABASE_TENANT", "agent_platform")
        self.database = os.getenv("DATABASE_NAME", "agent_engine")

    @property
    def full_database_name(self) -> str:
        """Get full database name with tenant prefix."""
        return f"{self.tenant}.{self.database}"


class Database:
    """Database connection manager with tenant support."""

    def __init__(self, config: DatabaseConfig | None = None):
        """Initialize database connection.

        Args:
            config: Database configuration. If None, loads from environment.
        """
        self.config = config or DatabaseConfig()
        self._connection: pymysql.Connection | None = None

    def connect(self) -> pymysql.Connection:
        """Create database connection.

        Returns:
            Database connection

        Raises:
            pymysql.Error: If connection fails
        """
        try:
            self._connection = pymysql.connect(
                host=self.config.host,
                port=self.config.port,
                user=self.config.user,
                password=self.config.password,
                database=self.config.full_database_name,
                charset="utf8mb4",
                cursorclass=pymysql.cursors.DictCursor,
                autocommit=False,
            )
            logger.info(
                f"Connected to database: {self.config.full_database_name} "
                f"at {self.config.host}:{self.config.port}"
            )
            return self._connection
        except pymysql.Error as e:
            logger.error(f"Failed to connect to database: {e}")
            raise

    def close(self):
        """Close database connection."""
        if self._connection:
            self._connection.close()
            self._connection = None
            logger.info("Database connection closed")

    @contextmanager
    def get_connection(self):
        """Get database connection as context manager.

        Yields:
            Database connection

        Example:
            with db.get_connection() as conn:
                cursor = conn.cursor()
                cursor.execute("SELECT * FROM users")
        """
        conn = self.connect()
        try:
            yield conn
        finally:
            conn.close()

    def execute(
        self, query: str, params: tuple | None = None, commit: bool = True
    ) -> int:
        """Execute a query.

        Args:
            query: SQL query
            params: Query parameters
            commit: Whether to commit transaction

        Returns:
            Number of affected rows

        Raises:
            pymysql.Error: If query fails
        """
        with self.get_connection() as conn:
            cursor = conn.cursor()
            try:
                cursor.execute(query, params)
                if commit:
                    conn.commit()
                rowcount: int = cursor.rowcount
                return rowcount
            except pymysql.Error as e:
                conn.rollback()
                logger.error(f"Query failed: {query}, error: {e}")
                raise
            finally:
                cursor.close()

    def fetchone(self, query: str, params: tuple | None = None) -> dict | None:
        """Fetch one row.

        Args:
            query: SQL query
            params: Query parameters

        Returns:
            Row as dictionary, or None if no results

        Raises:
            pymysql.Error: If query fails
        """
        with self.get_connection() as conn:
            cursor = conn.cursor()
            try:
                cursor.execute(query, params)
                result: dict | None = cursor.fetchone()
                return result
            except pymysql.Error as e:
                logger.error(f"Query failed: {query}, error: {e}")
                raise
            finally:
                cursor.close()

    def fetchall(self, query: str, params: tuple | None = None) -> list[dict]:
        """Fetch all rows.

        Args:
            query: SQL query
            params: Query parameters

        Returns:
            List of rows as dictionaries

        Raises:
            pymysql.Error: If query fails
        """
        with self.get_connection() as conn:
            cursor = conn.cursor()
            try:
                cursor.execute(query, params)
                results: list[dict] = cursor.fetchall()
                return results
            except pymysql.Error as e:
                logger.error(f"Query failed: {query}, error: {e}")
                raise
            finally:
                cursor.close()

    def health_check(self) -> bool:
        """Check database health.

        Returns:
            True if database is healthy, False otherwise
        """
        try:
            result = self.fetchone("SELECT 1 as health")
            return result is not None and result.get("health") == 1
        except Exception as e:
            logger.error(f"Health check failed: {e}")
            return False


# Global database instance
_db_instance: Database | None = None


def get_db() -> Database:
    """Get global database instance.

    Returns:
        Database instance
    """
    global _db_instance
    if _db_instance is None:
        _db_instance = Database()
    return _db_instance
