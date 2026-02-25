"""Security tests for SQL injection prevention."""

import pytest
from core.sandbox import Sandbox
from core.validation import validate_identifier


class TestSQLInjectionPrevention:
    """Test SQL injection prevention."""

    def test_validate_identifier_valid(self):
        """Test valid identifiers."""
        assert validate_identifier("my_table") == "my_table"
        assert validate_identifier("Table123") == "Table123"
        assert validate_identifier("_private") == "_private"
        assert validate_identifier("db.table", allow_dot=True) == "db.table"

    def test_validate_identifier_invalid(self):
        """Test invalid identifiers are rejected."""
        # SQL injection attempts
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("'; DROP TABLE users--")
        
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("table; DELETE FROM users")
        
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("table UNION SELECT * FROM passwords")
        
        # Special characters
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("table-name")
        
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("table name")
        
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier("table@name")

    def test_validate_identifier_must_start_with_letter(self):
        """Test identifier must start with letter or underscore."""
        with pytest.raises(ValueError, match="must start with letter or underscore"):
            validate_identifier("123table")
        
        # Dot is rejected by pattern check first
        with pytest.raises(ValueError, match="Invalid identifier"):
            validate_identifier(".table")

    def test_validate_identifier_length(self):
        """Test identifier length limits."""
        # Valid length
        assert validate_identifier("a" * 64) == "a" * 64
        
        # Too long
        with pytest.raises(ValueError, match="too long"):
            validate_identifier("a" * 65)

    def test_validate_identifier_empty(self):
        """Test empty identifier is rejected."""
        with pytest.raises(ValueError, match="cannot be empty"):
            validate_identifier("")

    def test_sandbox_create_sql_injection(self):
        """Test sandbox creation rejects SQL injection attempts."""
        from unittest.mock import Mock
        from sqlalchemy.orm import Session
        
        mock_db = Mock(spec=Session)
        sandbox = Sandbox(lambda: mock_db)
        
        # SQL injection attempts should be rejected
        with pytest.raises(ValueError):
            sandbox.create("'; DROP TABLE users--")
        
        with pytest.raises(ValueError):
            sandbox.create("test; DELETE FROM events")
        
        with pytest.raises(ValueError):
            sandbox.create("test UNION SELECT * FROM passwords")

    def test_sandbox_delete_sql_injection(self):
        """Test sandbox deletion rejects SQL injection attempts."""
        from unittest.mock import Mock
        from sqlalchemy.orm import Session
        
        mock_db = Mock(spec=Session)
        sandbox = Sandbox(lambda: mock_db)
        
        with pytest.raises(ValueError):
            sandbox.delete("'; DROP TABLE users--")

    def test_sandbox_use_sql_injection(self):
        """Test sandbox use rejects SQL injection attempts."""
        from unittest.mock import Mock
        from sqlalchemy.orm import Session
        
        mock_db = Mock(spec=Session)
        sandbox = Sandbox(lambda: mock_db)
        
        with pytest.raises(ValueError):
            sandbox.use("'; DROP TABLE users--")

    def test_add_table_sql_injection(self):
        """Test add_table rejects SQL injection via Branch._qualify → validate_identifier."""
        from unittest.mock import Mock
        from sqlalchemy.orm import Session
        
        mock_db = Mock(spec=Session)
        sandbox = Sandbox(lambda: mock_db)
        
        with pytest.raises(ValueError):
            sandbox.add_table("'; DROP TABLE users--", "t1")
        
        with pytest.raises(ValueError):
            sandbox.add_table("sandbox1", "'; DROP TABLE users--")
