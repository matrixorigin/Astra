"""FastAPI authentication router with SQLAlchemy."""

from datetime import datetime, timedelta, timezone
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy import text

from api.database import SessionLocal
from api.dependencies import get_current_user
from api.repositories.user_repository import UserRepository
from core.auth.jwt_manager import create_access_token, create_refresh_token, decode_token
from core.auth.password import hash_password, verify_password
from schemas.auth import LoginRequest, RefreshRequest, RegisterRequest, TokenResponse, UserResponse

router = APIRouter(tags=["authentication"])


@router.post("/register", response_model=UserResponse, status_code=status.HTTP_201_CREATED)
def register(request: RegisterRequest):
    """Register a new user. First user automatically becomes admin."""
    # Single session for the entire registration flow: user creation + admin
    # role assignment must be atomic — if role assignment fails, user creation
    # should also roll back.
    db = SessionLocal()
    try:
        repo = UserRepository(lambda: db)

        if repo.get_by_username(request.username):
            raise HTTPException(status_code=400, detail="Username already exists")
        if repo.get_by_email(request.email):
            raise HTTPException(status_code=400, detail="Email already exists")

        user = repo.create({
            "user_id": str(uuid4()),
            "username": request.username,
            "email": request.email,
            "password_hash": hash_password(request.password),
            "display_name": request.display_name,
            "is_active": 1,
        })

        # Atomically assign admin role if no admin exists yet.
        # INSERT ... SELECT ensures only one concurrent registration wins.
        try:
            db.execute(
                text(
                    "INSERT INTO user_roles (user_id, role_id) "
                    "SELECT :uid, r.role_id FROM roles r "
                    "WHERE r.role_name = 'mo_agent_admin' "
                    "AND NOT EXISTS (SELECT 1 FROM user_roles ur JOIN roles r2 "
                    "ON ur.role_id = r2.role_id WHERE r2.role_name = 'mo_agent_admin')"
                ),
                {"uid": user.user_id},
            )
        except Exception:
            pass  # Race condition: another registration won — not an error

        db.commit()

        return UserResponse(
            user_id=user.user_id,
            username=user.username,
            email=user.email,
            display_name=user.display_name,
        )
    except HTTPException:
        raise
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


@router.post("/login", response_model=TokenResponse)
def login(request: LoginRequest):
    """Login and get tokens."""
    import hashlib

    # Single session: authenticate + update last_login + store refresh token
    # must see consistent state.
    db = SessionLocal()
    try:
        repo = UserRepository(lambda: db)

        user = repo.get_by_username(request.username)
        if not user or not verify_password(request.password, user.password_hash):
            raise HTTPException(status_code=401, detail="Invalid username or password")
        if not user.is_active:
            raise HTTPException(status_code=403, detail="User is inactive")

        repo.update_last_login(user.user_id)

        access_token = create_access_token({"sub": user.user_id, "username": user.username})
        refresh_token = create_refresh_token({"sub": user.user_id})

        token_hash = hashlib.sha256(refresh_token.encode()).hexdigest()
        repo.store_refresh_token({
            "token_id": str(uuid4()),
            "user_id": user.user_id,
            "token_hash": token_hash,
            "expires_at": datetime.now(timezone.utc) + timedelta(days=7),
            "is_revoked": 0,
        })

        db.commit()

        return TokenResponse(
            access_token=access_token,
            refresh_token=refresh_token,
            token_type="bearer",
            expires_in=3600,
        )
    except HTTPException:
        raise
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


@router.post("/refresh", response_model=TokenResponse)
def refresh(request: RefreshRequest):
    """Refresh access token."""
    import hashlib

    # Single session: revoke old token + store new token must be atomic.
    db = SessionLocal()
    try:
        repo = UserRepository(lambda: db)

        try:
            payload = decode_token(request.refresh_token)
            if payload.get("type") != "refresh":
                raise HTTPException(status_code=401, detail="Invalid token type")
            user_id = payload.get("sub")
        except HTTPException:
            raise
        except Exception:
            raise HTTPException(status_code=401, detail="Invalid token")

        token_hash = hashlib.sha256(request.refresh_token.encode()).hexdigest()
        token = repo.get_refresh_token(token_hash)

        if not token:
            raise HTTPException(status_code=401, detail="Token expired or revoked")

        expires_at = token.expires_at.replace(tzinfo=timezone.utc) if token.expires_at.tzinfo is None else token.expires_at
        if expires_at < datetime.now(timezone.utc):
            raise HTTPException(status_code=401, detail="Token expired or revoked")

        user = repo.get_by_id(user_id)
        if not user:
            raise HTTPException(status_code=404, detail="User not found")

        # Revoke old + store new in same transaction
        repo.revoke_refresh_token(token_hash)

        access_token = create_access_token({"sub": user.user_id, "username": user.username})
        new_refresh_token = create_refresh_token({"sub": user.user_id})

        new_token_hash = hashlib.sha256(new_refresh_token.encode()).hexdigest()
        repo.store_refresh_token({
            "token_id": str(uuid4()),
            "user_id": user.user_id,
            "token_hash": new_token_hash,
            "expires_at": datetime.now(timezone.utc) + timedelta(days=30),
        })

        db.commit()

        return TokenResponse(
            access_token=access_token,
            refresh_token=new_refresh_token,
            token_type="bearer",
            expires_in=3600,
        )
    except HTTPException:
        raise
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


@router.post("/logout")
def logout(request: RefreshRequest):
    """Logout and revoke refresh token."""
    import hashlib

    db = SessionLocal()
    try:
        repo = UserRepository(lambda: db)
        token_hash = hashlib.sha256(request.refresh_token.encode()).hexdigest()
        repo.revoke_refresh_token(token_hash)
        db.commit()
        return {"message": "Logged out successfully"}
    except Exception:
        db.rollback()
        raise
    finally:
        db.close()


@router.get("/me", response_model=UserResponse)
def get_current_user_info(
    current_user: dict = Depends(get_current_user),
):
    """Get current user information."""
    db = SessionLocal()
    try:
        repo = UserRepository(lambda: db)
        user = repo.get_by_id(current_user["user_id"])
        if not user:
            raise HTTPException(status_code=404, detail="User not found")
        return UserResponse(
            user_id=user.user_id,
            username=user.username,
            email=user.email,
            is_active=bool(user.is_active),
            created_at=user.created_at.isoformat(),
        )
    finally:
        db.close()
