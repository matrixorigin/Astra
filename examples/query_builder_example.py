"""QueryBuilder Example - Flexible queries with filtering and window functions."""

from datetime import datetime, timedelta

from core.query import QueryBuilder
from sdk import Database

db = Database()

print("=== QueryBuilder Examples ===\n")

# 1. Basic filtering
print("1. Basic filtering")
query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'user_id', 'content', 'created_at')
    .where('user_id = %s', 'alice')
    .where_in('event_type', ['user_query', 'llm_response'])
    .order_by('created_at', desc=True)
    .limit(10)
    .build()
)
print(f"   Query: {query[:100]}...")
results = db.fetchall(query, params)
print(f"   Results: {len(results)} rows")

# 2. Time range filtering
print("\n2. Time range filtering")
end_time = datetime.now()
start_time = end_time - timedelta(days=7)

query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'created_at')
    .where_between('created_at', start_time, end_time)
    .limit(5)
    .build()
)
print(f"   Query: {query[:100]}...")
results = db.fetchall(query, params)
print(f"   Results: {len(results)} rows")

# 3. Window function - ROW_NUMBER
print("\n3. Window function - Message numbering per session")
query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'session_id', 'content')
    .add_window_function(
        'ROW_NUMBER()',
        'msg_num',
        partition_by='session_id',
        order_by='created_at'
    )
    .limit(10)
    .build()
)
print(f"   Query: {query[:150]}...")
results = db.fetchall(query, params)
if results:
    print(f"   Sample: session={results[0].get('session_id', 'N/A')}, msg_num={results[0].get('msg_num', 'N/A')}")

# 4. Window function - Rolling average
print("\n4. Window function - Rolling average quality score")
query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'quality_score')
    .add_window_function(
        'AVG(quality_score)',
        'rolling_avg',
        partition_by='user_id',
        order_by='created_at',
        frame='ROWS BETWEEN 5 PRECEDING AND CURRENT ROW'
    )
    .where('quality_score IS NOT NULL')
    .limit(10)
    .build()
)
print(f"   Query: {query[:150]}...")

# 5. Snapshot query with filtering
print("\n5. Time-travel query with filtering")
from sdk.git_for_data import GitForData

git = GitForData(db=db)
git.create_snapshot('demo_snapshot')

query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'event_type', 'created_at')
    .at_snapshot('demo_snapshot')
    .where('user_id = %s', 'alice')
    .order_by('created_at', desc=True)
    .limit(5)
    .build()
)
print(f"   Query: {query[:150]}...")
results = db.fetchall(query, params)
print(f"   Results: {len(results)} rows from snapshot")

git.drop_snapshot('demo_snapshot')

# 6. Pagination
print("\n6. Pagination")
query, params = (
    QueryBuilder('conversation_events')
    .select('event_id', 'content')
    .order_by('created_at', desc=True)
    .limit(10)
    .offset(20)  # Skip first 20, get next 10
    .build()
)
print(f"   Query: {query[:100]}...")
print(f"   Fetching page 3 (offset=20, limit=10)")

print("\n✅ Done!")
print("\nKey Features:")
print("- Flexible filtering (where, where_in, where_between)")
print("- Window functions (ROW_NUMBER, AVG, etc.)")
print("- Git for Data support (at_snapshot)")
print("- Pagination (limit, offset)")
print("- Column selection (avoid SELECT *)")
