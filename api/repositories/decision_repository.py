"""Decision repository for ORM-based data access."""

from collections.abc import Callable
import time

from sqlalchemy.engine import Connection, Engine
from sqlalchemy.orm import Session as DBSession
from sqlalchemy.orm import sessionmaker

from api.models import DecisionAudit as DecisionModel


class DecisionRepository:
    """Repository for decision audit operations."""

    def __init__(self, db_factory: Callable[[], DBSession]):
        self._db_factory = db_factory

    @property
    def db(self) -> DBSession:
        return self._db_factory()

    def create(self, decision_data: dict) -> DecisionModel:
        """Create decision record."""
        db = self.db
        decision = DecisionModel(**decision_data)
        db.add(decision)
        db.commit()
        row = db.query(DecisionModel).filter(DecisionModel.decision_id == decision.decision_id).first()
        if row is not None:
            return row
        bind = db.get_bind()
        if isinstance(bind, (Engine, Connection)):
            fresh_factory = sessionmaker(bind=bind, expire_on_commit=False)
            for attempt in range(6):
                fresh_db = fresh_factory()
                try:
                    visible = (
                        fresh_db.query(DecisionModel)
                        .filter(DecisionModel.decision_id == decision.decision_id)
                        .first()
                    )
                finally:
                    fresh_db.close()
                if visible is not None:
                    db.expire_all()
                    row = (
                        db.query(DecisionModel)
                        .filter(DecisionModel.decision_id == decision.decision_id)
                        .first()
                    )
                    if row is not None:
                        return row
                if attempt < 5:
                    time.sleep(0.03 * (attempt + 1))
        return decision

    def get_by_id(self, decision_id: str) -> DecisionModel | None:
        """Get decision by ID."""
        return self.db.query(DecisionModel).filter(DecisionModel.decision_id == decision_id).first()

    def get_by_id_with_user(self, decision_id: str, user_id: str) -> DecisionModel | None:
        """Get decision with user ownership check via session join."""
        from api.models import Session as SessionModel

        return (
            self.db.query(DecisionModel)
            .join(SessionModel, DecisionModel.session_id == SessionModel.session_id)
            .filter(DecisionModel.decision_id == decision_id, SessionModel.user_id == user_id)
            .first()
        )

    def list_by_session(
        self, session_id: str, limit: int = 50, offset: int = 0
    ) -> tuple[list[DecisionModel], int]:
        """List decisions by session."""
        query = self.db.query(DecisionModel).filter(DecisionModel.session_id == session_id)
        total = query.count()
        return query.order_by(DecisionModel.created_at.desc()).offset(offset).limit(
            limit
        ).all(), total

    def list_by_user(
        self, user_id: str, decision_type: str | None = None, limit: int = 50, offset: int = 0
    ) -> tuple[list[DecisionModel], int]:
        """List decisions by user with optional type filter."""
        from api.models import Session as SessionModel

        query = (
            self.db.query(DecisionModel)
            .join(SessionModel, DecisionModel.session_id == SessionModel.session_id)
            .filter(SessionModel.user_id == user_id)
        )
        if decision_type:
            query = query.filter(DecisionModel.decision_type == decision_type)
        total = query.count()
        return query.order_by(DecisionModel.created_at.desc()).offset(offset).limit(
            limit
        ).all(), total
