"""FastAPI agents router with SQLAlchemy optimization."""

from uuid import uuid4

from fastapi import APIRouter, Depends, HTTPException, status
from sqlalchemy.orm import Session

from api.database import get_db_session
from api.dependencies import get_current_user
from api.repositories import AgentRepository
from schemas.agent import AgentCreateRequest, AgentResponse, AgentUpdateRequest

router = APIRouter()


@router.post("", response_model=AgentResponse, status_code=status.HTTP_201_CREATED)
def create_agent(
    request: AgentCreateRequest,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Create a new agent."""
    repo = AgentRepository(db)
    
    agent_data = {
        "agent_id": str(uuid4()),
        "agent_name": request.agent_name,
        "agent_type": request.agent_type,
        "owner_user_id": current_user["user_id"],
        "agent_config": request.config,
        "is_active": True,
    }
    
    agent = repo.create(agent_data)
    
    return AgentResponse(
        agent_id=agent.agent_id,
        agent_name=agent.agent_name,
        agent_type=agent.agent_type,
        owner_user_id=agent.owner_user_id,
        config=agent.agent_config,
        is_active=agent.is_active,
        created_at=agent.created_at,
    )


@router.get("", response_model=dict)
def list_agents(
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
    agent_type: str | None = None,
    is_active: bool = True,
    limit: int = 50,
    offset: int = 0,
):
    """List agents with database-level filtering and pagination."""
    if limit > 100:
        limit = 100
    
    repo = AgentRepository(db)
    agents = repo.list_by_owner(
        owner_user_id=current_user["user_id"],
        agent_type=agent_type,
        is_active=is_active,
        limit=limit,
        offset=offset,
    )
    
    return {
        "agents": [
            AgentResponse(
                agent_id=a.agent_id,
                agent_name=a.agent_name,
                agent_type=a.agent_type,
                owner_user_id=a.owner_user_id,
                config=a.agent_config,
                is_active=a.is_active,
                created_at=a.created_at,
            )
            for a in agents
        ],
        "total": len(agents),
    }


@router.get("/{agent_id}", response_model=AgentResponse)
def get_agent(
    agent_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Get agent with ownership check at database level."""
    repo = AgentRepository(db)
    agent = repo.get_by_id(agent_id, owner_user_id=current_user["user_id"])
    
    if not agent:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )
    
    return AgentResponse(
        agent_id=agent.agent_id,
        agent_name=agent.agent_name,
        agent_type=agent.agent_type,
        owner_user_id=agent.owner_user_id,
        config=agent.agent_config,
        is_active=agent.is_active,
        created_at=agent.created_at,
    )


@router.put("/{agent_id}", response_model=AgentResponse)
def update_agent(
    agent_id: str,
    request: AgentUpdateRequest,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Update agent with ownership check in query."""
    repo = AgentRepository(db)
    
    updates = {}
    if request.agent_name is not None:
        updates["agent_name"] = request.agent_name
    if request.config is not None:
        updates["agent_config"] = request.config
    if request.is_active is not None:
        updates["is_active"] = request.is_active
    
    agent = repo.update(agent_id, current_user["user_id"], updates)
    
    if not agent:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )
    
    return AgentResponse(
        agent_id=agent.agent_id,
        agent_name=agent.agent_name,
        agent_type=agent.agent_type,
        owner_user_id=agent.owner_user_id,
        config=agent.agent_config,
        is_active=agent.is_active,
        created_at=agent.created_at,
    )


@router.delete("/{agent_id}", status_code=status.HTTP_204_NO_CONTENT)
def delete_agent(
    agent_id: str,
    current_user: dict = Depends(get_current_user),
    db: Session = Depends(get_db_session),
):
    """Delete agent with ownership check in query."""
    repo = AgentRepository(db)
    deleted = repo.delete(agent_id, current_user["user_id"])
    
    if not deleted:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )
