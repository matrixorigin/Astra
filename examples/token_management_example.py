"""Multi-repo management with token resolution example."""

from core.repos import (
    AccessScope,
    OwnerType,
    RepoRegistry,
    RepoType,
    TokenResolver,
    TokenType,
)
from sdk import Database

print("=" * 80)
print("Multi-Repo Management with Token Resolution")
print("=" * 80)

db = Database()
registry = RepoRegistry(db)
resolver = TokenResolver(db)

# ============================================================================
# 场景：MatrixOne 团队管理多个仓库，使用分层 token 管理
# ============================================================================

print("\n【场景】MatrixOne 团队的 token 管理策略")
print("-" * 80)

# 1. 创建租户级别的默认 token（所有团队成员共享）
print("\n1. 创建租户级别的默认 GitHub token")
tenant_token = resolver.create_token(
    token_type=TokenType.REPO,
    provider="github",
    secret_ref="vault://matrixone/github/tenant_token",
    scope_tenant_id="team_matrixone",
    metadata={"description": "Team default token (read-only)"},
)
print(f"✓ 租户 token: {tenant_token.token_id}")
print("  - 作用域: tenant_matrixone")
print("  - 用途: 团队成员默认使用")

# 2. 创建用户级别的 token（特定开发者需要更高权限）
print("\n2. 创建用户级别的 GitHub token")
user_token = resolver.create_token(
    token_type=TokenType.REPO,
    provider="github",
    secret_ref="vault://matrixone/github/alice_token",
    scope_user_id="developer_alice",
    metadata={"description": "Alice's personal token (write access)"},
)
print(f"✓ 用户 token: {user_token.token_id}")
print("  - 作用域: developer_alice")
print("  - 用途: 需要 write 权限的操作")

# 3. 创建仓库特定的 token（敏感仓库使用专用 token）
print("\n3. 创建仓库特定的 GitHub token")
repo_token = resolver.create_token(
    token_type=TokenType.REPO,
    provider="github",
    secret_ref="vault://matrixone/github/matrixone_repo_token",
    scope_user_id="developer_alice",
    metadata={"description": "MatrixOne repo specific token (admin)"},
)
print(f"✓ 仓库 token: {repo_token.token_id}")
print("  - 作用域: 特定仓库")
print("  - 用途: MatrixOne 主仓库的管理操作")

# 4. 注册仓库并关联 token
print("\n4. 注册仓库并关联 token")

# MatrixOne 主仓库（使用专用 token）
matrixone_repo = registry.create(
    repo_url="https://github.com/matrixorigin/matrixone",
    repo_type=RepoType.CODE,
    owner_id="developer_alice",
    owner_type=OwnerType.USER,
    access_scope=AccessScope.ADMIN,
    repo_group="matrixone-project",
    token_id=repo_token.token_id,  # 使用专用 token
    metadata={"default_branch": "main", "language": "go"},
)
print("✓ MatrixOne 主仓库")
print("  - Token: 仓库专用 token")

# CI 仓库（使用用户默认 token）
ci_repo = registry.create(
    repo_url="https://github.com/matrixorigin/mo-ci",
    repo_type=RepoType.CI,
    owner_id="developer_alice",
    owner_type=OwnerType.USER,
    access_scope=AccessScope.READ,
    repo_group="matrixone-project",
    # 不指定 token_id，将使用用户默认 token
    metadata={"ci_paths": [".github/workflows"]},
)
print("✓ CI 仓库")
print("  - Token: 用户默认 token")

# Tester 仓库（使用租户默认 token）
tester_repo = registry.create(
    repo_url="https://github.com/matrixorigin/mo-tester",
    repo_type=RepoType.TESTER,
    owner_id="team_matrixone",
    owner_type=OwnerType.TENANT,
    access_scope=AccessScope.READ,
    repo_group="matrixone-project",
    # 不指定 token_id，将使用租户默认 token
    metadata={"test_paths": ["cases/"]},
)
print("✓ Tester 仓库")
print("  - Token: 租户默认 token")

# 5. Token 解析演示
print("\n5. Token 解析演示（优先级：仓库 > 用户 > 租户 > 全局）")
print("-" * 80)

# 场景 A: 访问 MatrixOne 主仓库（有仓库专用 token）
print("\n场景 A: Alice 访问 MatrixOne 主仓库")
resolved = resolver.resolve_repo_token(
    user_id="developer_alice",
    tenant_id="team_matrixone",
    repo_id=matrixone_repo.repo_id,
)
print(f"✓ 解析到: {resolved.token_id}")
print("  - 优先级: 仓库专用 token（最高优先级）")
print("  - 说明: 使用仓库关联的专用 token")

# 场景 B: 访问 CI 仓库（无仓库 token，使用用户 token）
print("\n场景 B: Alice 访问 CI 仓库")
resolved = resolver.resolve_repo_token(
    user_id="developer_alice",
    tenant_id="team_matrixone",
    repo_id=ci_repo.repo_id,
)
print(f"✓ 解析到: {resolved.token_id}")
print("  - 优先级: 用户默认 token")
print("  - 说明: 仓库无专用 token，使用 Alice 的用户 token")

# 场景 C: 其他开发者访问 Tester 仓库（使用租户 token）
print("\n场景 C: Bob 访问 Tester 仓库")
resolved = resolver.resolve_repo_token(
    user_id="developer_bob",  # Bob 没有用户 token
    tenant_id="team_matrixone",
    repo_url="https://github.com/matrixorigin/mo-tester",
)
print(f"✓ 解析到: {resolved.token_id}")
print("  - 优先级: 租户默认 token")
print("  - 说明: Bob 无用户 token，使用团队共享 token")

# 6. Token 失效处理
print("\n6. Token 失效处理（模拟 GitHub API 401 错误）")
print("-" * 80)

print("\n模拟: Alice 的用户 token 过期，GitHub 返回 401")
resolver.deactivate_token(user_token.token_id)
print(f"✓ 已停用 token: {user_token.token_id}")

print("\n重新解析: Alice 访问 CI 仓库")
resolved = resolver.resolve_repo_token(
    user_id="developer_alice",
    tenant_id="team_matrixone",
    repo_id=ci_repo.repo_id,
)
if resolved:
    print(f"✓ 自动降级到: {resolved.token_id}")
    print("  - 优先级: 租户默认 token")
    print("  - 说明: 用户 token 失效，自动使用租户 token")
else:
    print("✗ 无可用 token")

# 7. 跨仓库操作示例
print("\n7. 跨仓库操作示例（Skills 使用场景）")
print("-" * 80)

group_repos = registry.list_by_group("matrixone-project")
print("\n项目组 'matrixone-project' 的所有仓库:")
for repo in group_repos:
    # 为每个仓库解析 token
    token = resolver.resolve_repo_token(
        user_id="developer_alice",
        tenant_id="team_matrixone",
        repo_id=repo.repo_id,
    )
    print(f"  - {repo.repo_type.value:8s}: {repo.repo_url}")
    print(f"    Token: {token.token_id if token else 'None'}")

print("\nSkills 可以:")
print("  1. 从 code 仓库获取最新提交（使用仓库专用 token）")
print("  2. 从 ci 仓库查询 CI 状态（使用用户 token）")
print("  3. 从 tester 仓库获取测试结果（使用租户 token）")
print("  4. 关联分析：提交 → CI 结果 → 测试覆盖率")

# Cleanup
print("\n8. 清理测试数据")
for repo in group_repos:
    registry.delete(repo.repo_id)
db.execute(
    "DELETE FROM tokens WHERE token_id IN (%s, %s, %s)",
    (tenant_token.token_id, user_token.token_id, repo_token.token_id),
)
print("✓ 清理完成")

print("\n" + "=" * 80)
print("Token 管理核心能力")
print("=" * 80)
print("""
1. 【分层 Token 管理】
   - 仓库级别：敏感仓库使用专用 token
   - 用户级别：开发者个人 token
   - 租户级别：团队共享 token
   - 全局级别：系统默认 token（可选）

2. 【优先级自动降级】
   - 仓库 > 用户 > 租户 > 全局
   - Token 失效时自动降级
   - 保证服务可用性

3. 【安全性】
   - 支持 Vault 集成（secret_ref）
   - Token 失效自动停用
   - 最小权限原则

4. 【跨仓库操作】
   - Skills 可以访问多个仓库
   - 每个仓库使用合适的 token
   - 统一的 token 解析接口
""")
print("=" * 80)
