"""Database connection pool for high performance."""

import pymysql
from pymysql.cursors import DictCursor
from queue import Queue, Empty
import threading
import os

from core.logging_config import get_logger

logger = get_logger(__name__)


class ConnectionPool:
    """Thread-safe connection pool."""
    
    _instance = None
    _lock = threading.Lock()
    
    def __new__(cls):
        if cls._instance is None:
            with cls._lock:
                if cls._instance is None:
                    cls._instance = super().__new__(cls)
        return cls._instance
    
    def __init__(self):
        if hasattr(self, '_initialized'):
            return
        
        self._initialized = True
        self.pool_size = int(os.getenv("DB_POOL_SIZE", "10"))
        self.max_overflow = int(os.getenv("DB_MAX_OVERFLOW", "5"))
        
        self.config = {
            'host': os.getenv("MATRIXONE_HOST", "localhost"),
            'port': int(os.getenv("MATRIXONE_PORT", "6001")),
            'user': os.getenv("MATRIXONE_USER", "root"),
            'password': os.getenv("MATRIXONE_PASSWORD", "111"),
            'database': os.getenv("MATRIXONE_DATABASE", "dev_agent"),
            'cursorclass': DictCursor,
            'autocommit': True
        }
        
        self._pool = Queue(maxsize=self.pool_size + self.max_overflow)
        self._current_size = 0
        self._size_lock = threading.Lock()
        
        # Pre-create connections
        for _ in range(self.pool_size):
            self._pool.put(self._create_connection())
            self._current_size += 1
        
        logger.info(f"Connection pool initialized: size={self.pool_size}")
    
    def _create_connection(self):
        """Create new connection."""
        return pymysql.connect(**self.config)
    
    def get_connection(self, timeout=5):
        """Get connection from pool."""
        try:
            conn = self._pool.get(block=False)
        except Empty:
            with self._size_lock:
                if self._current_size < self.pool_size + self.max_overflow:
                    conn = self._create_connection()
                    self._current_size += 1
                else:
                    conn = self._pool.get(block=True, timeout=timeout)
        
        # Verify alive
        try:
            conn.ping(reconnect=True)
        except:
            conn = self._create_connection()
        
        return conn
    
    def return_connection(self, conn):
        """Return connection to pool."""
        try:
            self._pool.put(conn, block=False)
        except:
            conn.close()
            with self._size_lock:
                self._current_size -= 1


# Global pool instance
_pool = None

def get_pool():
    """Get global connection pool."""
    global _pool
    if _pool is None:
        _pool = ConnectionPool()
    return _pool
