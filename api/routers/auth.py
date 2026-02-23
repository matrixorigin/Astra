"""FastAPI authentication router with SQLAlchemy."""

from datetime import datetime, timedelta, timezone
from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.repositories.user_repository import UserRepository
from core.auth.jwt_manager import create_access_token, create_refresh_token, decode_token
from core.auth.password import hash_password, verify_password
from schemas.auth import LoginRequest, RefreshRequest, RegisterRequest, TokenResponse, UserResponse

router = APIRouter(tags=["authentication"])


@router.post("/register", response_model=UserResponse, status_code=status.HTTP_201_CREATED)
def register(request: RegisterRequest, db: Session = Depends(get_db_session)):
    """Register a new user."""
    repo = UserRepository(db)

    # Check if username exists
    if repo.get_by_username(request.username):
        raise HTTPException(status_code=400, detail="Username already exists")

    # Check if email exists
    if repo.get_by_email(request.email):
        raise HTTPException(status_code=400, detail="Email already exists")

    # Create user
    user = repo.create({
        "user_id": str(uuid4()),
        "username": request.username,
        "email": request.email,
        "password_hash": hash_password(request.password),
        "display_name": request.display_name,
        "is_active": 1,
    })

    return UserResponse(
        user_id=user.user_id,
        username=user.username,
        email=user.email,
        display_name=user.display_name,
    )


@router.post("/login", response_model=TokenResponse)
def login(request: LoginRequest, db: Session = Depends(get_db_session)):
    """Login and get tokens."""
    repo = UserRepository(db)

    # Get user
    user = repo.get_by_username(request.username)
    if not user or not verify_password(request.password, user.password_hash):
        raise HTTPException(status_code=401, detail="Invalid username or password")

    if not user.is_active:
        raise HTTPException(status_code=403, detail="User is inactive")

    # Update last login
    repo.update_last_login(user.user_id)

    # Create tokens
    access_token = create_access_token({"sub": user.user_id, "username": user.username})
    refresh_token = create_refresh_token({"sub": user.user_id})

    # Store refresh token
    import hashlib
    token_hash = hashlib.sha256(refresh_token.encode()).hexdigest()
    repo.store_refresh_token({
        "token_id": str(uuid4()),
        "user_id": user.user_id,
        "token_hash": token_hash,
        "expires_at": datetime.now(timezone.utc) + timedelta(days=7),
        "is_revoked": 0,
    })

    return TokenResponse(
        access_token=access_token,
        refresh_token=refresh_token,
        token_type="bearer",
        expires_in=3600,
    )


@router.post("/refresh", response_model=TokenResponse)
def refresh(request: RefreshRequest, db: Session = Depends(get_db_session)):
    """Refresh access token."""
    repo = UserRepository(db)

    # Verify refresh token
    try:
        payload = decode_token(request.refresh_token)

        # Check token type
        if payload.get("type") != "refresh":
            raise HTTPException(status_code=401, detail="Invalid token type")

        user_id = payload.get("sub")
    except HTTPException:
        raise
    except Exception:
        raise HTTPException(status_code=401, detail="Invalid token")

    # Check if token is revoked
    import hashlib
    token_hash = hashlib.sha256(request.refresh_token.encode()).hexdigest()
    token = repo.get_refresh_token(token_hash)

    if not token:
        raise HTTPException(status_code=401, detail="Token expired or revoked")

    # Convert naive datetime to UTC-aware for comparison
    expires_at = token.expires_at.replace(tzinfo=timezone.utc) if token.expires_at.tzinfo is None else token.expires_at
    if expires_at < datetime.now(timezone.utc):
        raise HTTPException(status_code=401, detail="Token expired or revoked")

    # Get user
    user = repo.get_by_id(user_id)
    if not user:
        raise HTTPException(status_code=404, detail="User not found")

    # Revoke old refresh token
    repo.revoke_refresh_token(token_hash)

    # Create new tokens
    access_token = create_access_token({"sub": user.user_id, "username": user.username})
    new_refresh_token = create_refresh_token({"sub": user.user_id})

    # Store new refresh token
    new_token_hash = hashlib.sha256(new_refresh_token.encode()).hexdigest()
    repo.store_refresh_token({
        "token_id": str(uuid4()),
        "user_id": user.user_id,
        "token_hash": new_token_hash,
        "expires_at": datetime.now(timezone.utc) + timedelta(days=30),
    })

    return TokenResponse(
        access_token=access_token,
        refresh_token=new_refresh_token,
        token_type="bearer",
        expires_in=3600,
    )


@router.post("/logout")
def logout(request: RefreshRequest, db: Session = Depends(get_db_session)):
    """Logout and revoke refresh token."""
    repo = UserRepository(db)

    import hashlib
    token_hash = hashlib.sha256(request.refresh_token.encode()).hexdigest()
    repo.revoke_refresh_token(token_hash)

    return {"message": "Logged out successfully"}


@router.get("/me", response_model=UserResponse)
def get_current_user_info(
    db: Session = Depends(get_db_session),
    current_user: dict = Depends(get_current_user),
):
    """Get current user information."""
    repo = UserRepository(db)
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
