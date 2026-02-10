# TODO: Git for Data 优化

## 当前状态

✅ **已实现**:
- Snapshot 创建和管理
- Time Machine 只读查询（使用 `{SNAPSHOT = 'name'}` 语法）
- Sandbox 基础功能（基于 snapshot + restore）

⚠️ **已知限制**:
- Sandbox 使用 `RESTORE ACCOUNT`，影响整个账号
- 未使用 MatrixOne 的 Branch 功能
- 未使用 PITR（Point-in-Time Recovery）

## 优化计划

### 1. 使用 PITR 替代手动 Snapshot

**当前**: 手动创建 snapshot
```sql
CREATE SNAPSHOT my_checkpoint FOR ACCOUNT sys;
```

**优化**: 使用 PITR 自动保留历史
```sql
-- PITR 自动保留指定时间范围的历史
CREATE PITR my_pitr FOR ACCOUNT RANGE 7 'd';  -- 保留 7 天
CREATE PITR my_pitr FOR DATABASE db01 RANGE 1 'h';  -- 数据库级别，保留 1 小时
```

**优势**:
- 自动管理历史数据
- 不需要手动创建/删除 snapshot
- 更细粒度的控制（account/database/table 级别）

### 2. 使用数据库级别 Restore

**当前**: Account 级别 restore（影响所有数据库）
```sql
RESTORE ACCOUNT sys FROM SNAPSHOT my_snapshot;
```

**优化**: 数据库级别 restore（只影响特定数据库）
```sql
RESTORE DATABASE dev_agent FROM SNAPSHOT my_snapshot;
```

**优势**:
- 隔离性更好
- 不影响其他数据库
- 更安全的并发操作

### 3. 探索 Branch 功能（如果支持）

**设计文档推荐**: 使用 Branch 实现零拷贝沙盒

```sql
-- 创建分支（零拷贝）
CREATE BRANCH sandbox_exp FROM main AT SNAPSHOT my_snapshot;

-- 在分支上工作
USE sandbox_exp;
-- ... 进行实验 ...

-- 删除分支
DROP BRANCH sandbox_exp;
```

**优势**:
- 零拷贝，性能更好
- 完全隔离，互不干扰
- 不需要 restore 主库

### 4. 优化 Sandbox 实现

**当前实现**:
```python
def run_experiment(experiment_fn):
    checkpoint = create_snapshot("before")
    sandbox = create_snapshot("sandbox")
    restore_from_snapshot(sandbox)  # 影响整个 account
    result = experiment_fn()
    restore_from_snapshot(checkpoint)  # 恢复
    return result
```

**优化方案 A**: 使用数据库级别 restore
```python
def run_experiment(experiment_fn):
    checkpoint = create_snapshot("before")
    restore_database_from_snapshot("dev_agent", checkpoint)  # 只影响 dev_agent
    result = experiment_fn()
    return result
```

**优化方案 B**: 使用 Branch（如果支持）
```python
def run_experiment(experiment_fn):
    create_branch("sandbox_exp", from_snapshot="my_checkpoint")
    switch_to_branch("sandbox_exp")
    result = experiment_fn()
    switch_to_branch("main")
    drop_branch("sandbox_exp")
    return result
```

## 实施优先级

1. **高优先级**: 数据库级别 restore（安全性提升）
2. **中优先级**: PITR 集成（自动化管理）
3. **低优先级**: Branch 探索（需要验证 MatrixOne 支持情况）

## 参考

- MatrixOne 测试用例: `/home/xupeng/matrixone/test/distributed/cases/pitr/`
- MatrixOne 测试用例: `/home/xupeng/matrixone/test/distributed/cases/snapshot/`
- 设计文档: `docs/design/git-for-data-enhancements.md`
