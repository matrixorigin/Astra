"""Tests for permission checker."""

import pytest
from core.auth.permission_checker import PermissionChecker


class MockDB:
    """Mock database for testing."""
    
    def __init__(self):
        self.roles = {
            'admin': ['mo_agent_admin', 'mo_agent_user'],
            'alice': ['mo_agent_user'],
            'bob': []
        }
    
    def fetchone(self, query, params=None):
        if 'mo_role_grant' in query:
            user_id, role_name = params
            if user_id in self.roles and role_name in self.roles[user_id]:
                return {'cnt': 1}
            return {'cnt': 0}
        return None


def test_has_role():
    """Test role checking."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    assert checker.has_role('admin', 'mo_agent_admin')
    assert checker.has_role('alice', 'mo_agent_user')
    assert not checker.has_role('bob', 'mo_agent_user')


def test_is_admin():
    """Test admin role checking."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    assert checker.is_admin('admin')
    assert not checker.is_admin('alice')
    assert not checker.is_admin('bob')


def test_is_user():
    """Test user role checking."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    assert checker.is_user('admin')
    assert checker.is_user('alice')
    assert not checker.is_user('bob')


def test_can_manage_models():
    """Test model management permissions."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    # Admin can manage all scopes
    assert checker.can_manage_models('admin', 'global')
    assert checker.can_manage_models('admin', 'account', 'acme')
    assert checker.can_manage_models('admin', 'user', 'alice')
    
    # User can only manage their own user-scoped models
    assert checker.can_manage_models('alice', 'user', 'alice')
    assert not checker.can_manage_models('alice', 'user', 'bob')
    assert not checker.can_manage_models('alice', 'global')
    assert not checker.can_manage_models('alice', 'account', 'acme')
    
    # Non-user cannot manage anything
    assert not checker.can_manage_models('bob', 'user', 'bob')


def test_can_manage_skills():
    """Test skill management permissions."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    # Admin can manage global and account scopes
    assert checker.can_manage_skills('admin', 'global')
    assert checker.can_manage_skills('admin', 'account', 'acme')
    assert not checker.can_manage_skills('admin', 'user', 'alice')
    
    # User can manage their own user-scoped skills
    assert checker.can_manage_skills('alice', 'user', 'alice')
    assert not checker.can_manage_skills('alice', 'user', 'bob')
    assert not checker.can_manage_skills('alice', 'global')


def test_can_view_audit_logs():
    """Test audit log viewing permissions."""
    db = MockDB()
    checker = PermissionChecker(db)
    
    # Admin can view all logs
    assert checker.can_view_audit_logs('admin')
    assert checker.can_view_audit_logs('admin', 'alice')
    
    # User can view their own logs
    assert checker.can_view_audit_logs('alice', 'alice')
    assert not checker.can_view_audit_logs('alice', 'bob')
    assert not checker.can_view_audit_logs('alice')
