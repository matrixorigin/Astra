"""Test database schema matches SQLAlchemy models."""

import pytest
from sqlalchemy import inspect, text
from api.database import get_db_session
from api.models import Config, Token, LLMCallLog


@pytest.fixture
def db():
    """Get database session."""
    session = next(get_db_session())
    yield session
    session.close()


class TestConfigSchemaConsistency:
    """Verify Config model matches database schema."""
    
    def test_config_table_has_scope_fields(self, db):
        """Config table must have scope_type and scope_user_id columns."""
        result = db.execute(text("SHOW COLUMNS FROM configs")).fetchall()
        columns = {row[0] for row in result}
        
        assert "scope_type" in columns, "Missing scope_type column"
        assert "scope_user_id" in columns, "Missing scope_user_id column"
    
    def test_config_model_has_scope_fields(self):
        """Config model must define scope_type and scope_user_id."""
        assert hasattr(Config, "scope_type"), "Config model missing scope_type"
        assert hasattr(Config, "scope_user_id"), "Config model missing scope_user_id"
    
    def test_config_primary_key_is_config_id(self, db):
        """Config table primary key should be config_id."""
        result = db.execute(text("SHOW CREATE TABLE configs")).fetchone()
        create_table = result[1]
        
        assert "PRIMARY KEY (`config_id`)" in create_table, "config_id is not primary key"
    
    def test_config_unique_constraint_exists(self, db):
        """Config table should have unique constraint on logical key."""
        result = db.execute(text("SHOW CREATE TABLE configs")).fetchone()
        create_table = result[1]
        
        # Check for unique constraint on (key_name, scope_type, scope_user_id)
        assert "UNIQUE KEY" in create_table or "uq_config_scope" in create_table, \
            "Missing unique constraint on (key_name, scope_type, scope_user_id)"


class TestTokenSchemaConsistency:
    """Verify Token model matches database schema."""
    
    def test_token_table_has_scope_fields(self, db):
        """Token table must have scope fields."""
        result = db.execute(text("SHOW COLUMNS FROM tokens")).fetchall()
        columns = {row[0] for row in result}
        
        assert "scope_user_id" in columns, "Missing scope_user_id column"


class TestLLMCallLogSchemaConsistency:
    """Verify LLMCallLog model matches database schema."""
    
    def test_llm_call_logs_has_metadata_column(self, db):
        """LLM call logs table must have metadata column."""
        result = db.execute(text("SHOW COLUMNS FROM llm_call_logs")).fetchall()
        columns = {row[0] for row in result}
        
        assert "metadata" in columns, "Missing metadata column"


class TestModelQueryConsistency:
    """Verify queries use fields that exist in models."""
    
    def test_config_queries_use_defined_fields(self, db):
        """All Config queries should only use fields defined in the model."""
        # Test that we can query using scope fields
        try:
            db.execute(text("""
                SELECT * FROM configs 
                WHERE scope_type = 'global' AND scope_user_id IS NULL
                LIMIT 1
            """)).fetchone()
        except Exception as e:
            pytest.fail(f"Query failed with defined fields: {e}")
    
    def test_config_insert_with_all_required_fields(self, db):
        """Config insert should work with all required fields."""
        from uuid_utils import uuid7
        
        config_id = str(uuid7())
        try:
            db.execute(text("""
                INSERT INTO configs (config_id, key_name, scope_type, scope_user_id, value)
                VALUES (:config_id, :key_name, :scope_type, :scope_user_id, :value)
            """), {
                "config_id": config_id,
                "key_name": "test_schema_validation",
                "scope_type": "global",
                "scope_user_id": None,
                "value": "test_value"
            })
            db.commit()
            
            # Cleanup
            db.execute(text("DELETE FROM configs WHERE config_id = :id"), {"id": config_id})
            db.commit()
        except Exception as e:
            db.rollback()
            pytest.fail(f"Insert failed with all required fields: {e}")
