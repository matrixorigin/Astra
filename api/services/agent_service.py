"""Agent Service - 业务逻辑层"""

from datetime import datetime, timezone
from typing import Dict, List, Any, Optional
from sqlalchemy.orm import Session

from api.repositories import AgentRepository
from core.auth.audit_logger import AuditLogger
from core.auth.permission_checker import PermissionChecker


class AgentService:
    """Agent 业务服务"""
    
    def __init__(self, db_session: Session):
        self.db_session = db_session
        self.agent_repo = AgentRepository(db_session)
        self.audit = AuditLogger(db_session)
        self.permission = PermissionChecker(db_session)
    
    def create_agent(
        self,
        user_id: str,
        name: str,
        agent_config: Optional[Dict[str, Any]] = None,
        data_source: Optional[Dict[str, Any]] = None
    ) -> Dict[str, Any]:
        """创建 Agent
        
        Args:
            user_id: 用户ID
            name: Agent名称
            description: 描述
            agent_config: Agent配置
            data_source: 数据源配置
            
        Returns:
            Agent信息
            
        Raises:
            ValueError: 参数错误
        """
        # 1. 参数验证
        if not name or not name.strip():
            raise ValueError("Agent name 不能为空")
        
        # 2. 设置默认值
        if agent_config is None:
            agent_config = {}
        if data_source is None:
            data_source = {"type": "matrixone", "database": "dev_agent"}
        
        # 3. 创建 Agent
        try:
            from uuid_utils import uuid7
            
            agent_data = {
                "agent_id": str(uuid7()),  # 生成agent_id
                "agent_name": name,  # 使用正确的字段名
                "agent_type": "general",  # 设置默认类型
                "owner_user_id": user_id,
                "agent_config": agent_config,
                "data_source": data_source,
                "is_active": True
            }
            
            agent = self.agent_repo.create(agent_data)
            
            # 4. 审计日志
            self.audit.log(
                user_id=user_id,
                action="agent_create",
                resource_type="agent",
                resource_id=agent.agent_id,
                details={"name": name},
                status="success"
            )
            
            # 5. 返回结果
            return {
                "agent_id": agent.agent_id,
                "name": agent.agent_name,  # 映射字段名
                "agent_type": agent.agent_type,
                "owner_user_id": agent.owner_user_id,
                "agent_config": agent.agent_config,
                "data_source": agent.data_source,
                "is_active": agent.is_active,
                "created_at": agent.created_at.isoformat(),
                "updated_at": agent.updated_at.isoformat() if agent.updated_at else None
            }
            
        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="agent_create",
                resource_type="agent",
                resource_id=name,
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def get_agent(self, agent_id: str, user_id: str) -> Dict[str, Any]:
        """获取 Agent 信息
        
        Args:
            agent_id: Agent ID
            user_id: 用户ID
            
        Returns:
            Agent信息
            
        Raises:
            ValueError: Agent不存在或无权限
        """
        agent = self.agent_repo.get_by_id(agent_id)
        
        if not agent:
            raise ValueError(f"Agent {agent_id} 不存在")
        
        # 权限检查 - 只能访问自己的Agent
        if agent.owner_user_id != user_id:
            raise ValueError(f"无权限访问 Agent {agent_id}")
        
        return {
            "agent_id": agent.agent_id,
            "name": agent.agent_name,
            "agent_type": agent.agent_type,
            "owner_user_id": agent.owner_user_id,
            "agent_config": agent.agent_config,
            "data_source": agent.data_source,
            "is_active": agent.is_active,
            "created_at": agent.created_at.isoformat(),
            "updated_at": agent.updated_at.isoformat() if agent.updated_at else None
        }
    
    def list_agents(self, user_id: str) -> List[Dict[str, Any]]:
        """列出用户的 Agents
        
        Args:
            user_id: 用户ID
            
        Returns:
            Agent列表
        """
        agents = self.agent_repo.list_by_owner(user_id)
        
        return [
            {
                "agent_id": agent.agent_id,
                "name": agent.agent_name,
                "agent_type": agent.agent_type,
                "owner_user_id": agent.owner_user_id,
                "agent_config": agent.agent_config,
                "data_source": agent.data_source,
                "is_active": agent.is_active,
                "created_at": agent.created_at.isoformat(),
                "updated_at": agent.updated_at.isoformat() if agent.updated_at else None
            }
            for agent in agents
        ]
    
    def update_agent(
        self,
        agent_id: str,
        user_id: str,
        name: Optional[str] = None,
        agent_config: Optional[Dict[str, Any]] = None,
        data_source: Optional[Dict[str, Any]] = None,
        is_active: Optional[bool] = None
    ) -> Dict[str, Any]:
        """更新 Agent
        
        Args:
            agent_id: Agent ID
            user_id: 用户ID
            name: 新名称
            description: 新描述
            agent_config: 新配置
            data_source: 新数据源
            is_active: 是否激活
            
        Returns:
            更新后的Agent信息
            
        Raises:
            ValueError: Agent不存在或无权限
        """
        agent = self.agent_repo.get_by_id(agent_id)
        
        if not agent:
            raise ValueError(f"Agent {agent_id} 不存在")
        
        # 权限检查
        if agent.owner_user_id != user_id:
            raise ValueError(f"无权限修改 Agent {agent_id}")
        
        # 准备更新数据
        update_data = {}
        if name is not None:
            update_data["agent_name"] = name  # 使用正确的字段名
        if agent_config is not None:
            update_data["agent_config"] = agent_config
        if data_source is not None:
            update_data["data_source"] = data_source
        if is_active is not None:
            update_data["is_active"] = is_active
        
        if not update_data:
            # 没有更新内容，直接返回当前信息
            return self.get_agent(agent_id, user_id)
        
        try:
            updated_agent = self.agent_repo.update(agent_id, user_id, update_data)
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="agent_update",
                resource_type="agent",
                resource_id=agent_id,
                details=update_data,
                status="success"
            )
            
            return {
                "agent_id": updated_agent.agent_id,
                "name": updated_agent.agent_name,
                "agent_type": updated_agent.agent_type,
                "owner_user_id": updated_agent.owner_user_id,
                "agent_config": updated_agent.agent_config,
                "data_source": updated_agent.data_source,
                "is_active": updated_agent.is_active,
                "created_at": updated_agent.created_at.isoformat(),
                "updated_at": updated_agent.updated_at.isoformat() if updated_agent.updated_at else None
            }
            
        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="agent_update",
                resource_type="agent",
                resource_id=agent_id,
                details={"error": str(e)},
                status="failed"
            )
            raise
    
    def delete_agent(self, agent_id: str, user_id: str) -> None:
        """删除 Agent
        
        Args:
            agent_id: Agent ID
            user_id: 用户ID
            
        Raises:
            ValueError: Agent不存在或无权限
        """
        agent = self.agent_repo.get_by_id(agent_id)
        
        if not agent:
            raise ValueError(f"Agent {agent_id} 不存在")
        
        # 权限检查
        if agent.owner_user_id != user_id:
            raise ValueError(f"无权限删除 Agent {agent_id}")
        
        try:
            self.agent_repo.delete(agent_id, user_id)
            
            # 审计日志
            self.audit.log(
                user_id=user_id,
                action="agent_delete",
                resource_type="agent",
                resource_id=agent_id,
                details={"name": agent.agent_name},
                status="success"
            )
            
        except Exception as e:
            # 审计失败
            self.audit.log(
                user_id=user_id,
                action="agent_delete",
                resource_type="agent",
                resource_id=agent_id,
                details={"error": str(e)},
                status="failed"
            )
            raise
