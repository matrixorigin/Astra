"""Query package."""

# QueryBuilder has been removed due to SQL injection risks.
# Use SQLAlchemy ORM or parameterized queries with sqlalchemy.text() instead.
#
# Example:
#   from sqlalchemy import text
#   db.execute(text("SELECT * FROM events WHERE user_id = :user_id"), {"user_id": user_id})
