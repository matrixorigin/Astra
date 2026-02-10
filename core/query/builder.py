"""Lightweight query builder for flexible queries.

TODO: This is a SHORT-TERM solution. Replace with SQLAlchemy in the future for:
- Better type safety and IDE support
- ORM capabilities
- More robust query construction
- Better maintainability

Current implementation provides minimal functionality for:
- Dynamic filtering
- Window functions
- Git for Data (SNAPSHOT syntax)
- Column selection
"""

from typing import Any


class QueryBuilder:
    """Flexible SQL query builder."""

    def __init__(self, table: str):
        self.table = table
        self.columns: list[str] = []
        self.conditions: list[str] = []
        self.params: list[Any] = []
        self.order_clauses: list[str] = []
        self.limit_val: int | None = None
        self.offset_val: int | None = None
        self.snapshot_name: str | None = None
        self.window_funcs: list[str] = []

    def select(self, *cols: str) -> "QueryBuilder":
        """Select specific columns.

        Example:
            >>> qb.select('event_id', 'content', 'created_at')
        """
        self.columns.extend(cols)
        return self

    def where(self, condition: str, *params: Any) -> "QueryBuilder":
        """Add WHERE condition.

        Example:
            >>> qb.where('user_id = %s', user_id)
            >>> qb.where('created_at >= %s', start_time)
        """
        self.conditions.append(condition)
        self.params.extend(params)
        return self

    def where_in(self, column: str, values: list[Any]) -> "QueryBuilder":
        """Add WHERE IN condition.

        Example:
            >>> qb.where_in('event_type', ['user_query', 'llm_response'])
        """
        if not values:
            return self
        placeholders = ", ".join(["%s"] * len(values))
        self.conditions.append(f"{column} IN ({placeholders})")
        self.params.extend(values)
        return self

    def where_between(self, column: str, start: Any, end: Any) -> "QueryBuilder":
        """Add WHERE BETWEEN condition.

        Example:
            >>> qb.where_between('created_at', start_time, end_time)
        """
        self.conditions.append(f"{column} BETWEEN %s AND %s")
        self.params.extend([start, end])
        return self

    def order_by(self, column: str, desc: bool = False) -> "QueryBuilder":
        """Add ORDER BY clause.

        Example:
            >>> qb.order_by('created_at', desc=True)
        """
        direction = "DESC" if desc else "ASC"
        self.order_clauses.append(f"{column} {direction}")
        return self

    def limit(self, n: int) -> "QueryBuilder":
        """Add LIMIT clause."""
        self.limit_val = n
        return self

    def offset(self, n: int) -> "QueryBuilder":
        """Add OFFSET clause."""
        self.offset_val = n
        return self

    def at_snapshot(self, name: str) -> "QueryBuilder":
        """Query at specific snapshot (Git for Data).

        Example:
            >>> qb.at_snapshot('checkpoint1')
            >>> # Generates: FROM table {SNAPSHOT = 'checkpoint1'}
        """
        self.snapshot_name = name
        return self

    def add_window_function(
        self,
        func: str,
        alias: str,
        partition_by: str | None = None,
        order_by: str | None = None,
        order_desc: bool = False,
        frame: str | None = None,
    ) -> "QueryBuilder":
        """Add window function.

        Args:
            func: Function name (ROW_NUMBER, RANK, AVG, etc.)
            alias: Column alias
            partition_by: PARTITION BY column
            order_by: ORDER BY column
            order_desc: Order direction
            frame: Frame clause (e.g., 'ROWS BETWEEN 5 PRECEDING AND CURRENT ROW')

        Example:
            >>> # ROW_NUMBER() OVER (PARTITION BY session_id ORDER BY created_at) as row_num
            >>> qb.add_window_function('ROW_NUMBER()', 'row_num',
            ...                        partition_by='session_id',
            ...                        order_by='created_at')

            >>> # AVG(quality_score) OVER (PARTITION BY user_id ORDER BY created_at
            >>> #                          ROWS BETWEEN 5 PRECEDING AND CURRENT ROW) as rolling_avg
            >>> qb.add_window_function('AVG(quality_score)', 'rolling_avg',
            ...                        partition_by='user_id',
            ...                        order_by='created_at',
            ...                        frame='ROWS BETWEEN 5 PRECEDING AND CURRENT ROW')
        """
        over_clause = "OVER ("
        parts = []

        if partition_by:
            parts.append(f"PARTITION BY {partition_by}")

        if order_by:
            direction = "DESC" if order_desc else "ASC"
            parts.append(f"ORDER BY {order_by} {direction}")

        over_clause += " ".join(parts)

        if frame:
            over_clause += f" {frame}"

        over_clause += ")"

        self.window_funcs.append(f"{func} {over_clause} AS {alias}")
        return self

    def build(self) -> tuple[str, tuple[Any, ...]]:
        """Build SQL query and parameters.

        Returns:
            tuple: (query_string, parameters)
        """
        # SELECT clause
        if self.columns or self.window_funcs:
            cols = []
            if self.columns:
                cols.extend(self.columns)
            else:
                cols.append("*")
            cols.extend(self.window_funcs)
            select_clause = ", ".join(cols)
        else:
            select_clause = "*"

        # FROM clause with optional SNAPSHOT
        if self.snapshot_name:
            from_clause = f"{self.table} {{SNAPSHOT = '{self.snapshot_name}'}}"
        else:
            from_clause = self.table

        query = f"SELECT {select_clause} FROM {from_clause}"

        # WHERE clause
        if self.conditions:
            query += " WHERE " + " AND ".join(self.conditions)

        # ORDER BY clause
        if self.order_clauses:
            query += " ORDER BY " + ", ".join(self.order_clauses)

        # LIMIT clause
        if self.limit_val is not None:
            query += f" LIMIT {self.limit_val}"

        # OFFSET clause
        if self.offset_val is not None:
            query += f" OFFSET {self.offset_val}"

        return query, tuple(self.params)
