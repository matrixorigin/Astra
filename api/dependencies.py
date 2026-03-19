"""Authentication dependencies with SQLAlchemy."""

from fastapi import Depends, HTTPException
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy.orm import Session

from api.database import SessionLocal
from api.database import get_db_session
from api.repositories.user_repository import UserRepository
from core.auth.jwt_manager import decode_token, verify_token_type

security = HTTPBearer()


def _load_user_with_fresh_session(user_id: str, username: str | None) -> object | None:
    """Retry user lookup on a fresh session for MatrixOne visibility gaps."""
    fresh_db = SessionLocal()
    try:
        repo = UserRepository(lambda: fresh_db)
        user = repo.get_by_id(user_id)
        if user is None and username:
            user = repo.get_by_username(username)
        return user
    finally:
        fresh_db.close()


def get_current_user(
    credentials: HTTPAuthorizationCredentials = Depends(security),
    db: Session = Depends(get_db_session),
) -> dict:
    """Get current authenticated user."""
    try:
        payload = decode_token(credentials.credentials)

        if not verify_token_type(payload, "access"):
            raise HTTPException(status_code=401, detail="Invalid token type")

        user_id = payload.get("sub")
        username = payload.get("username")

        if not user_id:
            raise HTTPException(status_code=401, detail="Invalid token")

        # Verify user exists
        repo = UserRepository(lambda: db)
        user = repo.get_by_id(user_id)
        if user is None and username:
            user = repo.get_by_username(username)
        if user is None:
            user = _load_user_with_fresh_session(user_id, username)

        if not user:
            raise HTTPException(status_code=401, detail="User not found")

        return {"user_id": user_id, "username": username}

    except HTTPException:
        raise
    except Exception:
        raise HTTPException(status_code=401, detail="Could not validate credentials")


def get_current_user_id(
    current_user: dict = Depends(get_current_user),
) -> str:
    """Extract user_id from authenticated user."""
    return current_user["user_id"]


def get_db_factory():
    """Return a DbFactory (callable that yields a DB session)."""
    from api.database import SessionLocal

    def _factory():
        return SessionLocal()

    return _factory
