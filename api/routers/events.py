"""FastAPI events router."""

from fastapi import APIRouter, Depends, HTTPException, status

from api.dependencies import get_current_user, get_db
from core.events.event_logger import EventLogger
from core.events.session_manager import SessionManager
from sdk import Database
from schemas.event import EventCreateRequest, EventListResponse, EventResponse

router = APIRouter()


def get_event_logger(db: Database = Depends(get_db)) -> EventLogger:
    """Get event logger dependency."""
    return EventLogger(db)


def get_session_manager(db: Database = Depends(get_db)) -> SessionManager:
    """Get session manager dependency."""
    return SessionManager(db)


@router.post("", response_model=EventResponse, status_code=status.HTTP_201_CREATED)
def create_event(
    request: EventCreateRequest,
    current_user: dict = Depends(get_current_user),
    event_logger: EventLogger = Depends(get_event_logger),
    session_manager: SessionManager = Depends(get_session_manager),
):
    """Create a new event."""
    # Verify session ownership
    session = session_manager.get_session(request.session_id)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    if session.user_id != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to add events to this session",
        )
    
    # Create event based on type
    if request.event_type == "user_query":
        event = event_logger.create_user_query(
            user_id=current_user["user_id"],
            session_id=request.session_id,
            content=request.content,
            metadata=request.metadata,
        )
    elif request.event_type == "llm_response":
        event = event_logger.create_llm_response(
            user_id=current_user["user_id"],
            session_id=request.session_id,
            content=request.content,
            agent_id=request.metadata.get("agent_id") if request.metadata else None,
            agent_version=request.metadata.get("agent_version") if request.metadata else None,
            metadata=request.metadata,
        )
    else:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Unsupported event type: {request.event_type}",
        )
    
    return EventResponse(
        event_id=event.event_id,
        session_id=event.session_id,
        user_id=event.user_id,
        event_type=event.event_type,
        content=event.content,
        created_at=event.created_at,
        metadata=event.metadata,
        parent_event_id=event.parent_event_id,
        causal_chain_id=event.causal_chain_id,
    )


@router.get("", response_model=EventListResponse)
def list_events(
    session_id: str,
    current_user: dict = Depends(get_current_user),
    event_logger: EventLogger = Depends(get_event_logger),
    session_manager: SessionManager = Depends(get_session_manager),
    event_type: str | None = None,
    limit: int = 100,
    offset: int = 0,
):
    """List events for a session with pagination and filtering.
    
    - **session_id**: Session ID (required)
    - **event_type**: Filter by type (user_query, llm_response)
    - **limit**: Max results (default 100, max 500)
    - **offset**: Skip N results for pagination
    """
    if limit > 500:
        limit = 500
    
    # Verify session ownership
    session = session_manager.get_session(session_id)
    if not session:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Session not found",
        )
    
    if session.user_id != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to view events for this session",
        )
    
    # Get events
    events = event_logger.get_session_events(session_id, limit=limit + offset)
    
    # Apply offset
    events = events[offset:]
    
    # Filter by event_type if provided
    if event_type:
        events = [e for e in events if e.event_type == event_type]
    
    # Apply limit
    events = events[:limit]
    
    event_responses = [
        EventResponse(
            event_id=e.event_id,
            session_id=e.session_id,
            user_id=e.user_id,
            event_type=e.event_type,
            content=e.content,
            created_at=e.created_at,
            metadata=e.metadata,
            parent_event_id=e.parent_event_id,
            causal_chain_id=e.causal_chain_id,
        )
        for e in events
    ]
    
    return EventListResponse(
        events=event_responses,
        total=len(event_responses),
    )


@router.get("/{event_id}", response_model=EventResponse)
def get_event(
    event_id: str,
    current_user: dict = Depends(get_current_user),
    event_logger: EventLogger = Depends(get_event_logger),
):
    """Get event by ID."""
    event = event_logger.get_event(event_id)
    
    if not event:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Event not found",
        )
    
    # Verify ownership
    if event.user_id != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to view this event",
        )
    
    return EventResponse(
        event_id=event.event_id,
        session_id=event.session_id,
        user_id=event.user_id,
        event_type=event.event_type,
        content=event.content,
        created_at=event.created_at,
        metadata=event.metadata,
        parent_event_id=event.parent_event_id,
        causal_chain_id=event.causal_chain_id,
    )
