"""Authentication dependencies with SQLAlchemy."""

from fastapi import Depends, HTTPException
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.repositories.user_repository import UserRepository
from core.auth.jwt_manager import decode_token, verify_token_type

security = HTTPBearer()


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
        repo = UserRepository(db)
        user = repo.get_by_id(user_id)

        if not user:
            raise HTTPException(status_code=401, detail="User not found")

        return {"user_id": user_id, "username": username}

    except HTTPException:
        raise
    except Exception:
        raise HTTPException(status_code=401, detail="Could not validate credentials")
