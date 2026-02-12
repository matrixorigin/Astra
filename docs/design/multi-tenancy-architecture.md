# 多租户和数据源架构设计

## 概述

mo-agent-engine 采用**租户隔离 + 灵活数据源**的架构，实现：
1. Core Service 与用户数据完全隔离
2. 支持多种数据源（MatrixOne, MySQL, PostgreSQL）
3. Sandbox 能力可选（仅 MatrixOne）
4. 权限在数据库层面管理，应用层不感知

## 架构图

```
┌─────────────────────────────────────────────────────────────┐
│  MatrixOne Cluster                                          │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ sys 租户 (运维/系统管理员)                              │  │
│  │  - 集群管理                                            │  │
│  │  - 最高权限                                            │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ mo_agent_core 租户 (Core Service)                     │  │
│  │  ✓ users, agents, sessions, events                    │  │
│  │  ✓ skills, context, memory, tokens                    │  │
│  │  ✓ sandbox_metadata (元数据管理)                       │  │
│  │  ✓ 不感知用户数据库                                    │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ user_alice 租户 (Alice 的数据)                        │  │
│  │  - Alice 的业务数据                                   │  │
│  │  - Alice 创建的 sandbox                               │  │
│  │  - 权限由 MatrixOne RBAC 管理                         │  │
│  └──────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐  │
│  │ user_bob 租户 (Bob 的数据)                            │  │
│  │  - Bob 的业务数据                                     │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│  外部数据库 (可选)                                           │
│  - MySQL, PostgreSQL, 其他 MatrixOne 集群                  │
│  - User Agent 可以连接任何数据库                            │
│  - 如果不是 MatrixOne，则无 Sandbox 能力                    │
└─────────────────────────────────────────────────────────────┘
```

## 核心原则

### 1. Core Service 租户隔离

**Core Service 只管理平台数据**:
- 用户账号 (users)
- Agent 定义 (agents)
- 会话和事件 (sessions, events)
- 技能和上下文 (skills, context, memory)
- Sandbox 元数据 (sandbox_metadata)

**不管理用户业务数据**:
- 用户的业务表
- 用户的实验数据
- 用户的 Sandbox 内容

### 2. 用户数据源灵活配置

**Agent 绑定数据源**:
```python
agent = {
    "agent_id": "alice_agent_1",
    "agent_name": "Alice's Data Agent",
    "owner_user_id": "alice",
    "data_source": {
        "type": "matrixone",      # 或 "mysql", "postgres"
        "host": "mo-cluster",
        "port": 6001,
        "user": "user_alice",     # Alice 的租户/用户
        "password": "***",        # 加密存储
        "database": "alice_data"
    }
}
```

**支持的数据源类型**:
- `matrixone` - 支持 Sandbox, Time Travel, Git for Data
- `mysql` - 基础功能，无 Sandbox
- `postgres` - 基础功能，无 Sandbox

### 3. Sandbox 权限 - 数据库层面

**Sandbox 不管权限**:
```python
# Sandbox 直接执行 SQL
# 权限检查由数据库完成
sandbox.create("alice_exp_1")
# → CREATE DATABASE alice_exp_1 CLONE alice_data
# 如果 user_alice 没有权限，MatrixOne 会拒绝
```

**权限完全由数据库管理**:
- MatrixOne: 使用 RBAC (GRANT/REVOKE)
- MySQL: 使用 GRANT
- PostgreSQL: 使用 GRANT

**Core Service 只管理元数据**:
```python
# 记录 Sandbox 创建
core_db.execute("""
    INSERT INTO sandbox_metadata
    (sandbox_name, user_id, data_source, created_at, expires_at)
    VALUES (%s, %s, %s, NOW(), NOW() + INTERVAL 24 HOUR)
""")
```

### 4. 生命周期管理 - Core Service

**自动清理过期 Sandbox**:
```python
# 定时任务（每小时）
def cleanup_expired_sandboxes():
    expired = core_db.fetchall("""
        SELECT sandbox_name, data_source 
        FROM sandbox_metadata 
        WHERE expires_at < NOW() AND status = 'active'
    """)
    
    for row in expired:
        # 连接用户数据库
        user_db = Database(**json.loads(row["data_source"]))
        
        # 删除 Sandbox
        try:
            Sandbox(db=user_db).delete(row["sandbox_name"])
        except:
            pass  # 可能已被手动删除
        
        # 更新元数据
        core_db.execute("""
            UPDATE sandbox_metadata 
            SET status = 'deleted', deleted_at = NOW()
            WHERE sandbox_name = %s
        """, (row["sandbox_name"],))
```

## 数据模型

### Agent 模型

```python
class Agent(Base):
    __tablename__ = "agents"
    agent_id = Column(String(36), primary_key=True)
    agent_name = Column(String(100), nullable=False)
    agent_type = Column(String(50), nullable=False)
    owner_user_id = Column(String(36), nullable=False, index=True)
    agent_config = Column(JSON)
    data_source = Column(JSON)  # 新增：数据源配置
    # {
    #   "type": "matrixone",
    #   "host": "...",
    #   "port": 6001,
    #   "user": "...",
    #   "password": "...",  # 加密
    #   "database": "..."
    # }
    is_active = Column(TINYINT(1), server_default="1")
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
```

### Sandbox 元数据模型

```python
class SandboxMetadata(Base):
    __tablename__ = "sandbox_metadata"
    sandbox_name = Column(String(255), primary_key=True)
    user_id = Column(String(36), nullable=False, index=True)  # 新增
    data_source = Column(JSON, nullable=False)  # 新增：数据源配置
    description = Column(Text)
    created_by = Column(String(255))
    source_database = Column(String(255))
    source_snapshot = Column(String(255))
    status = Column(String(32), default="active")
    created_at = Column(DateTime, default=func.now())
    updated_at = Column(DateTime, default=func.now(), onupdate=func.now())
    expires_at = Column(DateTime)  # 新增：过期时间
    deleted_at = Column(DateTime)  # 新增：删除时间
    tables = Column(JSON)
    tags = Column(JSON)
```

## API 设计

### Sandbox API

```python
# POST /sandbox
{
    "name": "alice_exp_1",
    "description": "实验环境",
    "data_source": {  # 可选，默认使用 Agent 的 data_source
        "type": "matrixone",
        "host": "...",
        "user": "...",
        "database": "..."
    },
    "ttl_hours": 24  # 生命周期（小时）
}

# Response
{
    "sandbox_name": "alice_exp_1",
    "user_id": "alice",
    "status": "active",
    "created_at": "2026-02-12T10:00:00Z",
    "expires_at": "2026-02-13T10:00:00Z",
    "capabilities": ["time_travel", "git_for_data"]  # 根据数据源类型
}
```

### Agent API

```python
# POST /agents
{
    "agent_name": "Alice's Agent",
    "agent_type": "data_analyst",
    "data_source": {
        "type": "matrixone",
        "host": "mo-cluster",
        "port": 6001,
        "user": "user_alice",
        "password": "***",
        "database": "alice_data"
    }
}
```

## 权限模型

### 应用层权限（Core Service）

**简单的所有权检查**:
```python
# 只能操作自己的资源
def delete_sandbox(sandbox_name: str, user_id: str):
    sandbox = get_sandbox(sandbox_name)
    if sandbox.user_id != user_id:
        raise PermissionError("只能删除自己的 Sandbox")
    # ...
```

**不依赖数据库 RBAC**:
- Core Service 不查询 `mo_catalog.mo_user_grant`
- 不依赖 `mo_agent_admin`, `mo_agent_user` 角色
- 纯应用层的 JWT + owner check

### 数据库层权限（用户数据库）

**完全由数据库管理**:
```sql
-- MatrixOne 示例
CREATE USER user_alice IDENTIFIED BY '***';
GRANT ALL ON DATABASE alice_data TO user_alice;
GRANT CREATE DATABASE ON ACCOUNT TO user_alice;  -- 允许创建 Sandbox

-- MySQL 示例
CREATE USER 'alice'@'%' IDENTIFIED BY '***';
GRANT ALL PRIVILEGES ON alice_data.* TO 'alice'@'%';
```

**Sandbox 操作自动受限**:
- 如果用户没有 `CREATE DATABASE` 权限，创建 Sandbox 会失败
- 如果用户没有 `DROP DATABASE` 权限，删除 Sandbox 会失败
- Core Service 不需要检查，数据库会拒绝

## 实现路线图

### Phase 1: 数据模型更新 ✅
- [x] Agent 添加 `data_source` 字段
- [x] SandboxMetadata 添加 `user_id`, `data_source`, `expires_at`
- [x] 数据库迁移脚本

### Phase 2: Sandbox Service 重构 ✅
- [x] 支持动态数据源
- [x] 移除 RBAC 依赖
- [x] 添加生命周期管理

### Phase 3: API 更新 ✅
- [x] Sandbox API 支持 data_source 参数
- [x] Agent API 支持 data_source 配置
- [x] 返回 capabilities 信息

### Phase 4: 定时任务
- [ ] 实现 Sandbox 自动清理
- [ ] 监控和告警

## 优势

### ✅ 职责清晰
- Core Service: 平台管理
- User Database: 业务数据
- 完全解耦

### ✅ 灵活扩展
- 支持多种数据源
- Sandbox 能力可选
- 易于添加新数据源类型

### ✅ 权限简单
- 应用层: JWT + owner check
- 数据库层: 原生 RBAC
- 不需要复杂的权限系统

### ✅ 安全隔离
- 租户级别隔离
- 数据源级别隔离
- Sandbox 级别隔离

## 迁移指南

### 从旧架构迁移

**旧架构**:
- 所有数据在一个数据库
- Sandbox 假设固定数据库
- 权限依赖 MatrixOne RBAC

**新架构**:
- Core Service 独立租户
- Sandbox 支持动态数据源
- 权限在数据库层面

**迁移步骤**:
1. 创建 `mo_agent_core` 租户
2. 迁移平台数据到 Core Service
3. 为每个用户创建独立租户/数据库
4. 更新 Agent 配置添加 `data_source`
5. 更新 Sandbox 元数据

## 总结

这个架构实现了：
- **清晰的职责分离** - Core Service vs User Data
- **灵活的数据源** - MatrixOne, MySQL, PostgreSQL
- **简单的权限模型** - 应用层 + 数据库层
- **可选的高级能力** - Sandbox, Time Travel (仅 MatrixOne)

符合实际使用场景，易于理解和维护。
