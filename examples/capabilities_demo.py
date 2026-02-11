"""
mo-agent-engine 核心能力演示
展示实际应用场景
"""

from core.events.causal_chain import CausalChainManager
from core.events.event_logger import EventLogger
from core.events.event_reader import EventReader
from core.events.session_manager import SessionManager
from core.repos import AccessScope, OwnerType, RepoRegistry, RepoType
from core.sandbox import Branch, Sandbox
from sdk import Database

print("=" * 80)
print("mo-agent-engine 核心能力演示")
print("=" * 80)

db = Database()

# ============================================================================
# 场景 1: 完整的对话追踪和因果链分析
# ============================================================================
print("\n【场景 1】完整的对话追踪和因果链分析")
print("-" * 80)
print("用例：用户报告了一个 bug，需要追溯完整的对话历史和决策链")

session_mgr = SessionManager(db)
logger = EventLogger(db)
reader = EventReader(db)
chain_mgr = CausalChainManager(db)

# 创建会话
session = session_mgr.create_session(user_id="developer_alice", metadata={"project": "matrixone"})
print(f"✓ 创建会话: {session.session_id}")

# 用户提问
user_event = logger.create_user_query(
    user_id="developer_alice",
    session_id=session.session_id,
    content="MatrixOne 的 PITR 功能如何使用？",
)
print(f"✓ 用户提问: {user_event.event_id}")

# LLM 响应
llm_event = logger.create_llm_response(
    user_id="developer_alice",
    session_id=session.session_id,
    content="PITR (Point-in-Time Recovery) 使用方法：CREATE PITR...",
    agent_id="dev-agent",
    agent_version="0.1.0",
    parent_event_id=user_event.event_id,
    causal_chain_id=user_event.causal_chain_id,
    token_usage={"prompt": 150, "completion": 300, "total": 450},
)
print(f"✓ LLM 响应: {llm_event.event_id}")

# 追踪因果链
chain = chain_mgr.get_chain(user_event.causal_chain_id)
print("\n因果链分析:")
print(f"  - 链 ID: {user_event.causal_chain_id}")
print(f"  - 事件数: {len(chain)}")
print(f"  - 事件类型: {[e.event_type for e in chain]}")

# 获取链统计
summary = chain_mgr.get_chain_summary(user_event.causal_chain_id)
print(f"  - Token 使用: {summary['total_tokens']}")
print(f"  - 事件分布: {summary['event_types']}")

# 验证完整性
validation = chain_mgr.validate_chain_integrity(user_event.causal_chain_id)
print(f"  - 链完整性: {'✓ 完整' if validation['is_valid'] else '✗ 破损'}")

# Cleanup
db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session.session_id,))
db.execute("DELETE FROM sessions WHERE session_id = %s", (session.session_id,))

# ============================================================================
# 场景 2: 多仓库协同管理
# ============================================================================
print("\n【场景 2】多仓库协同管理")
print("-" * 80)
print("用例：管理 MatrixOne 项目的多个仓库（代码、CI、测试）")

registry = RepoRegistry(db)

# 注册 MatrixOne 项目的所有仓库
repos_config = [
    {
        "url": "https://github.com/matrixorigin/matrixone",
        "type": RepoType.CODE,
        "access": AccessScope.WRITE,
        "metadata": {"default_branch": "main", "language": "go"},
    },
    {
        "url": "https://github.com/matrixorigin/mo-ci",
        "type": RepoType.CI,
        "access": AccessScope.READ,
        "metadata": {"ci_paths": [".github/workflows"]},
    },
    {
        "url": "https://github.com/matrixorigin/mo-tester",
        "type": RepoType.TESTER,
        "access": AccessScope.READ,
        "metadata": {"test_paths": ["cases/"]},
    },
]

created_repos = []
for config in repos_config:
    repo = registry.create(
        repo_url=config["url"],
        repo_type=config["type"],
        owner_id="team_matrixone",
        owner_type=OwnerType.TENANT,
        access_scope=config["access"],
        repo_group="matrixone-project",
        metadata=config["metadata"],
    )
    created_repos.append(repo)
    print(f"✓ 注册仓库: {repo.repo_url} ({repo.repo_type.value})")

# 按组查询所有相关仓库
group_repos = registry.list_by_group("matrixone-project")
print("\n项目仓库组 'matrixone-project':")
print(f"  - 总数: {len(group_repos)}")
for repo in group_repos:
    print(f"  - {repo.repo_type.value:8s}: {repo.repo_url}")

# 场景：Skills 可以跨仓库操作
print("\n跨仓库操作示例:")
code_repo = next(r for r in group_repos if r.repo_type == RepoType.CODE)
ci_repo = next(r for r in group_repos if r.repo_type == RepoType.CI)
print(f"  1. 从 {code_repo.repo_type.value} 仓库获取最新提交")
print(f"  2. 从 {ci_repo.repo_type.value} 仓库查询 CI 状态")
print("  3. 关联分析：提交 → CI 结果")

# Cleanup
for repo in created_repos:
    registry.delete(repo.repo_id)

# ============================================================================
# 场景 3: 时光机 - 对话重放和调试
# ============================================================================
print("\n【场景 3】时光机 - 对话重放和调试")
print("-" * 80)
print("用例：用户报告 AI 给出了错误答案，需要重现当时的上下文")

from core.replay.time_machine import TimeMachine

tm = TimeMachine(db)

# 创建测试会话
session = session_mgr.create_session(user_id="user_bob")
event1 = logger.create_user_query(
    user_id="user_bob",
    session_id=session.session_id,
    content="如何优化 MatrixOne 查询性能？",
)
event2 = logger.create_llm_response(
    user_id="user_bob",
    session_id=session.session_id,
    content="建议使用索引...",
    agent_id="dev-agent",
    agent_version="0.1.0",
    parent_event_id=event1.event_id,
    causal_chain_id=event1.causal_chain_id,
)

print("✓ 创建测试会话和对话")
print(f"  - 会话 ID: {session.session_id}")
print("  - 事件数: 2")

# 列出所有检查点
checkpoints = tm.list_checkpoints()
print(f"\n当前系统检查点: {len(checkpoints)} 个")

# Cleanup
db.execute("DELETE FROM conversation_events WHERE session_id = %s", (session.session_id,))
db.execute("DELETE FROM sessions WHERE session_id = %s", (session.session_id,))

# ============================================================================
# 场景 4: 沙盒实验 - 安全的数据实验
# ============================================================================
print("\n【场景 4】沙盒实验 - 安全的数据实验")
print("-" * 80)
print("用例：测试新的 prompt 版本，不影响生产数据")

sandbox = Sandbox(db=db)

# 列出现有沙盒
sandboxes = sandbox.list_sandboxes()
print(f"✓ 当前沙盒数量: {len(sandboxes)}")

print("\n沙盒能力:")
print("  - 零拷贝克隆（秒级创建）")
print("  - 完全隔离（不影响生产）")
print("  - 支持检查点和恢复")
print("  - 可以添加/删除特定表")

# ============================================================================
# 场景 5: 数据分支 - Git-like 工作流
# ============================================================================
print("\n【场景 5】数据分支 - Git-like 工作流")
print("-" * 80)
print("用例：在分支上测试数据变更，对比差异后合并")

branch = Branch(db=db)

print("✓ 数据分支能力:")
print("  - 创建分支（零拷贝）")
print("  - 对比差异（diff）")
print("  - 合并变更（merge）")
print("  - 冲突处理（error/skip/accept）")

# ============================================================================
# 总结
# ============================================================================
print("\n" + "=" * 80)
print("核心能力总结")
print("=" * 80)
print("""
1. 【对话追踪】完整的事件溯源 + 因果链分析
   - 追踪每个决策的来龙去脉
   - Token 使用统计
   - 链完整性验证

2. 【多仓库管理】跨仓库协同操作
   - 统一管理 code/ci/tester/docs 仓库
   - repo_group 逻辑分组
   - Skills 可以跨仓库操作

3. 【时光机】对话重放和调试
   - 创建检查点
   - 时光旅行查询
   - 重现历史上下文

4. 【沙盒实验】安全的数据实验
   - 零拷贝克隆（秒级）
   - 完全隔离
   - 不影响生产数据

5. 【数据分支】Git-like 数据工作流
   - 创建分支
   - 对比差异
   - 合并变更

这些能力为构建"可重现、可追溯、可实验"的 AI Agent 提供了坚实基础。
""")
print("=" * 80)
