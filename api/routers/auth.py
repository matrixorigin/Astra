"""FastAPI authentication router."""

from datetime import datetime, timedelta, timezone

from fastapi import APIRouter, Depends, HTTPException, status
from jwt.exceptions import InvalidTokenError

from api.dependencies import get_db
from core.auth.jwt_manager import (
    JWTConfig,
    create_access_token,
    create_refresh_token,
    decode_token,
    verify_token_type,
)
from core.auth.user_manager import UserManager
from sdk import Database
from schemas.auth import (
    LoginRequest,
    RefreshRequest,
    RegisterRequest,
    TokenResponse,
    UserResponse,
)

router = APIRouter(
    tags=["authentication"],
    responses={401: {"description": "Invalid credentials"}},
)


def get_user_manager(db: Database = Depends(get_db)) -> UserManager:
    """Get user manager dependency."""
    return UserManager(db)


@router.post("/register", response_model=UserResponse, status_code=status.HTTP_201_CREATED)
def register(
    request: RegisterRequest,
    user_manager: UserManager = Depends(get_user_manager),
):
    """Register a new user.
    
    Create a new user account with username, email, and password.
    Username must be unique. Returns user details (password excluded).
    """
    try:
        user = user_manager.create_user(
            username=request.username,
            email=request.email,
            password=request.password,
            display_name=request.display_name,
        )
        return UserResponse(**user)
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=str(e),
        )


@router.post("/login", response_model=TokenResponse)
def login(
    request: LoginRequest,
    user_manager: UserManager = Depends(get_user_manager),
):
    """Login and get access token.

    Args:
        request: Login request
        user_manager: User manager dependency

    Returns:
        Access and refresh tokens

    Raises:
        HTTPException: If authentication fails
    """
    user = user_manager.authenticate_user(request.username, request.password)

    if not user:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid username or password",
            headers={"WWW-Authenticate": "Bearer"},
        )

    # Create tokens
    config = JWTConfig()
    access_token = create_access_token(
        {
            "sub": user["user_id"],
            "username": user["username"],
            "email": user["email"],
        },
        config,
    )
    refresh_token = create_refresh_token({"sub": user["user_id"]}, config)

    # Store refresh token
    expires_at = datetime.now(timezone.utc) + timedelta(days=config.refresh_token_expire_days)
    user_manager.store_refresh_token(user["user_id"], refresh_token, expires_at)

    return TokenResponse(
        access_token=access_token,
        refresh_token=refresh_token,
        expires_in=config.access_token_expire_minutes * 60,
    )


@router.post("/refresh", response_model=TokenResponse)
def refresh(
    request: RefreshRequest,
    user_manager: UserManager = Depends(get_user_manager),
):
    """Refresh access token.

    Args:
        request: Refresh request
        user_manager: User manager dependency

    Returns:
        New access and refresh tokens

    Raises:
        HTTPException: If refresh token is invalid
    """
    try:
        # Decode and verify refresh token
        config = JWTConfig()
        payload = decode_token(request.refresh_token, config)

        if not verify_token_type(payload, "refresh"):
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED,
                detail="Invalid token type",
            )

        # Verify token in database
        user_id = user_manager.verify_refresh_token(request.refresh_token)
        if not user_id:
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED,
                detail="Invalid or expired refresh token",
            )

        # Get user
        user = user_manager.get_user_by_id(user_id)
        if not user:
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED,
                detail="User not found",
            )

        # Create new tokens
        access_token = create_access_token(
            {
                "sub": user["user_id"],
                "username": user["username"],
                "email": user["email"],
            },
            config,
        )
        new_refresh_token = create_refresh_token({"sub": user["user_id"]}, config)

        # Revoke old refresh token and store new one
        user_manager.revoke_refresh_token(request.refresh_token)
        expires_at = datetime.now(timezone.utc) + timedelta(days=config.refresh_token_expire_days)
        user_manager.store_refresh_token(user["user_id"], new_refresh_token, expires_at)

        return TokenResponse(
            access_token=access_token,
            refresh_token=new_refresh_token,
            expires_in=config.access_token_expire_minutes * 60,
        )

    except InvalidTokenError:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid token",
        )


@router.post("/logout", status_code=status.HTTP_200_OK)
def logout(
    request: RefreshRequest,
    user_manager: UserManager = Depends(get_user_manager),
):
    """Logout and revoke refresh token.

    Args:
        request: Logout request with refresh token
        user_manager: User manager dependency

    Returns:
        Success message
    """
    user_manager.revoke_refresh_token(request.refresh_token)
    return {"message": "Logged out successfully"}
