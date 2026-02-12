# 简化权限模型设计

## 核心原则

mo-agent-engine 使用**简单的应用层权限模型**，不依赖数据库 RBAC：

1. **JWT 认证** - 验证用户身份
2. **资源所有权** - 基于 `owner_user_id` 的授权  
3. **数据库权限** - 由数据库本身管理

## 权限架构

```
Client (JWT Token)
    ↓
API Layer (验证 JWT → user_id)
    ↓
Service Layer (检查 owner_user_id)
    ↓
Repository Layer (操作 Core Service 数据)
    ↓
User Database (数据库权限控制)
```

## 应用层权限

### 资源所有权模型

```python
# 所有资源都有 owner_user_id
class Agent:
    owner_user_id: str  # 谁拥有这个 Agent

class Session:
    user_id: str        # 谁拥有这个 Session

class Sandbox:
    user_id: str        # 谁创建的这个 Sandbox
```

### 权限检查逻辑

```python
def delete_agent(agent_id: str, user_id: str):
    agent = agent_repo.get(agent_id)
    
    # 简单检查：只能操作自己的资源
    if agent.owner_user_id != user_id:
        raise PermissionError("只能删除自己的 Agent")
    
    agent_repo.delete(agent_id)
```

**不需要**:
- ❌ 查询 `mo_catalog.mo_user_grant`
- ❌ 检查 `mo_agent_admin` 角色
- ❌ 复杂的权限矩阵

## 数据库层权限

### 完全由数据库管理

```python
# Sandbox 操作
sandbox.create("alice_exp_1")
# → CREATE DATABASE alice_exp_1 CLONE alice_data

# 权限检查：
# - 如果 user_alice 有权限 → 成功
# - 如果 user_alice 没权限 → 数据库拒绝
# - Core Service 不需要预先检查
```

### 错误处理

```python
try:
    sandbox.create(name)
    # 成功 - 记录审计日志
    audit.log(user_id, "sandbox_create", name, status="success")
except DatabaseError as e:
    # 失败 - 记录审计日志
    audit.log(user_id, "sandbox_create", name, status="failed", error=str(e))
    raise PermissionError(f"创建 Sandbox 失败: {e}")
```

## 实现

### 1. 删除 RBAC 依赖

```python
# 旧代码 ❌
if not self.permission.has_role(user_id, "mo_agent_user"):
    raise PermissionError("权限不足")

# 新代码 ✅
# 不检查角色，直接执行
# 权限由数据库控制
```

### 2. 简化权限检查

```python
class SandboxService:
    def create_sandbox(self, name: str, user_id: str):
        # 不检查应用层权限
        # 直接创建，让数据库决定权限
        try:
            self.sandbox.create(name)
            self.audit.log(user_id, "sandbox_create", name, "success")
        except Exception as e:
            self.audit.log(user_id, "sandbox_create", name, "failed")
            raise
    
    def delete_sandbox(self, name: str, user_id: str):
        # 检查所有权
        sandbox = self.get_sandbox_metadata(name)
        if sandbox.user_id != user_id:
            raise PermissionError("只能删除自己的 Sandbox")
        
        # 删除
        self.sandbox.delete(name)
```

## 总结

**这个权限模型**:
- ✅ 简单直接
- ✅ 不依赖特定数据库
- ✅ 易于理解和维护
- ✅ 符合实际使用场景

**核心思想**: 应用管应用的权限，数据库管数据库的权限。
