"""DbConsumer — base class for components that need short-lived DB sessions."""

from contextlib import contextmanager
from typing import Callable, Iterator

from sqlalchemy.orm import Session


class DbConsumer:
    """Base for components that acquire DB sessions on demand.

    Instead of holding a long-lived session, each operation acquires a session
    from the factory, uses it, and returns it to the pool immediately.

    Usage::

        class MyComponent(DbConsumer):
            def do_work(self):
                with self._db() as db:
                    db.execute(...)
                    db.commit()
    """

    def __init__(self, db_factory: Callable[[], Session]):
        self._db_factory = db_factory

    @contextmanager
    def _db(self) -> Iterator[Session]:
        db = self._db_factory()
        try:
            yield db
        except Exception:
            db.rollback()
            raise
        finally:
            db.close()
