"""FastAPI agents router."""

from fastapi import APIRouter, Depends, HTTPException, status

from api.dependencies import get_current_user
from core.agent.agent_manager import AgentManager
from db.database import Database, get_db
from schemas.agent import AgentCreateRequest, AgentResponse, AgentUpdateRequest

router = APIRouter()


def get_agent_manager(db: Database = Depends(get_db)) -> AgentManager:
    """Get agent manager dependency."""
    return AgentManager(db)


@router.post("", response_model=AgentResponse, status_code=status.HTTP_201_CREATED)
def create_agent(
    request: AgentCreateRequest,
    current_user: dict = Depends(get_current_user),
    agent_manager: AgentManager = Depends(get_agent_manager),
):
    """Create a new agent.

    Args:
        request: Agent creation request
        current_user: Current authenticated user
        agent_manager: Agent manager dependency

    Returns:
        Created agent

    Raises:
        HTTPException: If creation fails
    """
    try:
        agent = agent_manager.create_agent(
            agent_name=request.agent_name,
            owner_user_id=current_user["user_id"],
            agent_type=request.agent_type,
            config=request.config,
        )
        return AgentResponse(**agent)
    except ValueError as e:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=str(e),
        )


@router.get("", response_model=list[AgentResponse])
def list_agents(
    current_user: dict = Depends(get_current_user),
    agent_manager: AgentManager = Depends(get_agent_manager),
):
    """List user's agents.

    Args:
        current_user: Current authenticated user
        agent_manager: Agent manager dependency

    Returns:
        List of agents
    """
    agents = agent_manager.list_agents(owner_user_id=current_user["user_id"])
    return [AgentResponse(**agent) for agent in agents]


@router.get("/{agent_id}", response_model=AgentResponse)
def get_agent(
    agent_id: str,
    current_user: dict = Depends(get_current_user),
    agent_manager: AgentManager = Depends(get_agent_manager),
):
    """Get agent by ID.

    Args:
        agent_id: Agent ID
        current_user: Current authenticated user
        agent_manager: Agent manager dependency

    Returns:
        Agent

    Raises:
        HTTPException: If agent not found or user doesn't own it
    """
    agent = agent_manager.get_agent(agent_id)

    if not agent:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )

    if agent["owner_user_id"] != current_user["user_id"]:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail="Not authorized to access this agent",
        )

    return AgentResponse(**agent)


@router.put("/{agent_id}", response_model=AgentResponse)
def update_agent(
    agent_id: str,
    request: AgentUpdateRequest,
    current_user: dict = Depends(get_current_user),
    agent_manager: AgentManager = Depends(get_agent_manager),
):
    """Update agent.

    Args:
        agent_id: Agent ID
        request: Agent update request
        current_user: Current authenticated user
        agent_manager: Agent manager dependency

    Returns:
        Updated agent

    Raises:
        HTTPException: If agent not found or user doesn't own it
    """
    # Verify ownership
    if not agent_manager.verify_agent_owner(agent_id, current_user["user_id"]):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )

    # Update agent
    success = agent_manager.update_agent(
        agent_id=agent_id,
        agent_name=request.agent_name,
        config=request.config,
        is_active=request.is_active,
    )

    if not success:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )

    # Return updated agent
    agent = agent_manager.get_agent(agent_id)
    return AgentResponse(**agent)


@router.delete("/{agent_id}", status_code=status.HTTP_204_NO_CONTENT)
def delete_agent(
    agent_id: str,
    current_user: dict = Depends(get_current_user),
    agent_manager: AgentManager = Depends(get_agent_manager),
):
    """Delete agent.

    Args:
        agent_id: Agent ID
        current_user: Current authenticated user
        agent_manager: Agent manager dependency

    Raises:
        HTTPException: If agent not found or user doesn't own it
    """
    # Verify ownership
    if not agent_manager.verify_agent_owner(agent_id, current_user["user_id"]):
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )

    # Delete agent
    success = agent_manager.delete_agent(agent_id)

    if not success:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail="Agent not found",
        )
