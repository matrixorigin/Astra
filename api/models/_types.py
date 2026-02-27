"""Custom SQLAlchemy types for MatrixOne compatibility."""

from typing import Any, Optional

from sqlalchemy import JSON as _SA_JSON
from sqlalchemy.engine import Dialect
from sqlalchemy.types import TypeDecorator


class NullableJSON(TypeDecorator):
    """JSON type that stores Python None as SQL NULL, not JSON 'null'.

    MatrixOne's MySQL-compatible dialect serialises Python None to the
    JSON literal ``null`` via the impl's bind_processor.  This wrapper
    short-circuits that: when the value is None we return None directly
    (SQL NULL) and only delegate to the impl for real values.
    """

    impl = _SA_JSON
    cache_ok = True

    def bind_processor(self, dialect: Dialect):
        impl_processor = self.impl_instance.bind_processor(dialect)

        def process(value: Optional[Any]) -> Optional[str]:
            if value is None:
                return None  # SQL NULL, not JSON 'null'
            return impl_processor(value) if impl_processor else value

        return process
