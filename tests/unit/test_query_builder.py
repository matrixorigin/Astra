"""Tests for QueryBuilder."""

import pytest

from core.query import QueryBuilder


def test_basic_select():
    """Test basic SELECT."""
    query, params = (
        QueryBuilder('conversation_events')
        .select('event_id', 'content')
        .build()
    )
    
    assert query == "SELECT event_id, content FROM conversation_events"
    assert params == ()


def test_where_conditions():
    """Test WHERE conditions."""
    query, params = (
        QueryBuilder('conversation_events')
        .select('event_id', 'content')
        .where('user_id = %s', 'alice')
        .where('created_at >= %s', '2024-01-01')
        .build()
    )
    
    assert 'WHERE user_id = %s AND created_at >= %s' in query
    assert params == ('alice', '2024-01-01')


def test_where_in():
    """Test WHERE IN."""
    query, params = (
        QueryBuilder('conversation_events')
        .where_in('event_type', ['user_query', 'llm_response'])
        .build()
    )
    
    assert 'WHERE event_type IN (%s, %s)' in query
    assert params == ('user_query', 'llm_response')


def test_where_between():
    """Test WHERE BETWEEN."""
    query, params = (
        QueryBuilder('conversation_events')
        .where_between('created_at', '2024-01-01', '2024-12-31')
        .build()
    )
    
    assert 'WHERE created_at BETWEEN %s AND %s' in query
    assert params == ('2024-01-01', '2024-12-31')


def test_order_and_limit():
    """Test ORDER BY and LIMIT."""
    query, params = (
        QueryBuilder('conversation_events')
        .order_by('created_at', desc=True)
        .limit(100)
        .build()
    )
    
    assert 'ORDER BY created_at DESC' in query
    assert 'LIMIT 100' in query


def test_snapshot():
    """Test snapshot query."""
    query, params = (
        QueryBuilder('conversation_events')
        .select('event_id')
        .at_snapshot('checkpoint1')
        .build()
    )
    
    assert "FROM conversation_events {SNAPSHOT = 'checkpoint1'}" in query


def test_window_function_row_number():
    """Test ROW_NUMBER window function."""
    query, params = (
        QueryBuilder('conversation_events')
        .select('event_id', 'content')
        .add_window_function(
            'ROW_NUMBER()',
            'row_num',
            partition_by='session_id',
            order_by='created_at'
        )
        .build()
    )
    
    assert 'ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY created_at ASC) AS row_num' in query


def test_window_function_with_frame():
    """Test window function with frame."""
    query, params = (
        QueryBuilder('conversation_events')
        .add_window_function(
            'AVG(quality_score)',
            'rolling_avg',
            partition_by='user_id',
            order_by='created_at',
            frame='ROWS BETWEEN 5 PRECEDING AND CURRENT ROW'
        )
        .build()
    )
    
    assert 'AVG(quality_score) OVER' in query
    assert 'PARTITION BY user_id' in query
    assert 'ROWS BETWEEN 5 PRECEDING AND CURRENT ROW' in query


def test_complex_query():
    """Test complex query with multiple features."""
    query, params = (
        QueryBuilder('conversation_events')
        .select('event_id', 'user_id', 'content', 'created_at')
        .where('user_id = %s', 'alice')
        .where_in('event_type', ['user_query', 'llm_response'])
        .where_between('created_at', '2024-01-01', '2024-12-31')
        .add_window_function('ROW_NUMBER()', 'row_num', partition_by='session_id', order_by='created_at')
        .at_snapshot('checkpoint1')
        .order_by('created_at', desc=True)
        .limit(100)
        .offset(10)
        .build()
    )
    
    # Verify all parts are present
    assert 'SELECT event_id, user_id, content, created_at' in query
    assert 'ROW_NUMBER()' in query
    assert "FROM conversation_events {SNAPSHOT = 'checkpoint1'}" in query
    assert 'WHERE user_id = %s AND event_type IN (%s, %s) AND created_at BETWEEN %s AND %s' in query
    assert 'ORDER BY created_at DESC' in query
    assert 'LIMIT 100' in query
    assert 'OFFSET 10' in query
    assert params == ('alice', 'user_query', 'llm_response', '2024-01-01', '2024-12-31')
