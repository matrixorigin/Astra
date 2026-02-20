# Skill Embedding 缓存优化 (TODO)

## 问题

`ModernSkillSelector.__init__` 每次构造都调用 `embed_fn` N 次（N = 技能数）：
- 6 个技能 = 6 次 API 调用
- 50 个技能 = 50 次 API 调用

如果使用 OpenAI embedding API，频繁构造会很慢且浪费成本。

## 当前状态

- ✅ `SkillPipeline` 在 `ChatLoop` 中是长生命周期对象，所以当前规模（<20 技能）无感
- ⚠️ 如果技能数 >20 或频繁构造，性能会成为问题

## 优化方案

### 方案 1: 内存缓存（简单）

```python
import hashlib

def _skill_content_hash(skill: SkillMetadata) -> str:
    """Generate hash of skill content for cache invalidation."""
    content = f"{skill.name}|{skill.description}|{','.join(skill.triggers)}"
    return hashlib.sha256(content.encode()).hexdigest()[:16]

# 全局缓存
_EMBEDDING_CACHE: dict[tuple[str, str, str], list[float]] = {}

class ModernSkillSelector:
    def __init__(self, ...):
        for skill in skills:
            content_hash = _skill_content_hash(skill)
            cache_key = (skill.name, skill.version, content_hash)
            if cache_key in _EMBEDDING_CACHE:
                embedding = _EMBEDDING_CACHE[cache_key]
            else:
                embedding = embed_fn(skill_text)
                _EMBEDDING_CACHE[cache_key] = embedding
            self._index.add(skill.name, embedding)
```

**优势**:
- 简单，无需 DB
- 进程内共享

**劣势**:
- 重启丢失
- 多进程不共享

**缓存失效策略**:
- ✅ 技能版本变更时自动失效（cache key 包含 version）
- ✅ **技能内容变更时自动失效（cache key 包含 content_hash）**
- ✅ 进程重启时清空
- 可选：LRU 限制大小（如 1000 个技能）

**并发安全**:
```python
import threading
_CACHE_LOCK = threading.Lock()

with _CACHE_LOCK:
    if cache_key not in _EMBEDDING_CACHE:
        _EMBEDDING_CACHE[cache_key] = embed_fn(skill_text)
```

### 方案 2: DB 缓存（持久化）

```python
import hashlib

def _skill_content_hash(skill: SkillMetadata) -> str:
    """Generate hash of skill content for cache invalidation."""
    content = f"{skill.name}|{skill.description}|{','.join(skill.triggers)}"
    return hashlib.sha256(content.encode()).hexdigest()[:16]

# 新表
CREATE TABLE skill_embeddings (
    skill_name VARCHAR(100),
    skill_version VARCHAR(50),
    content_hash VARCHAR(16),
    embedding_vector JSON,
    created_at TIMESTAMP,
    PRIMARY KEY (skill_name, skill_version, content_hash)
);

class ModernSkillSelector:
    def __init__(self, ...):
        # 批量查询已有 embedding
        keys = [(s.name, s.version, _skill_content_hash(s)) for s in skills]
        cached = db.query(SkillEmbedding).filter(
            (name, version, content_hash) in keys
        ).all()
        
        # 只对缺失的调用 embed_fn
        for skill in skills:
            content_hash = _skill_content_hash(skill)
            if (skill.name, skill.version, content_hash) in cached:
                embedding = cached[(skill.name, skill.version, content_hash)]
            else:
                embedding = embed_fn(skill_text)
                db.add(SkillEmbedding(
                    skill_name=skill.name,
                    skill_version=skill.version,
                    content_hash=content_hash,
                    embedding_vector=embedding
                ))
```

**优势**:
- 持久化，重启不丢失
- 多进程共享
- 可审计

**劣势**:
- 需要 DB 表
- 稍复杂

**缓存失效策略**:
- ✅ 技能版本变更时自动失效（PRIMARY KEY 包含 version）
- ✅ **技能内容变更时自动失效（PRIMARY KEY 包含 content_hash）**
- 可选：定期清理旧版本（保留最近 N 个版本）
- 可选：TTL（如 30 天后过期）

**并发安全**:
- DB 事务保证原子性
- 使用 `INSERT ... ON CONFLICT DO NOTHING` 避免重复插入
- 读操作无需锁（embedding 不可变）

### 方案 3: 混合（推荐）

内存 LRU 缓存 + DB 持久化：
- 热数据在内存（LRU 1000 条）
- 冷数据在 DB
- 最佳性能

**缓存失效策略**:
```python
from functools import lru_cache

@lru_cache(maxsize=1000)
def _get_embedding_cached(skill_name: str, skill_version: str) -> list[float]:
    # 1. 查 DB
    cached = db.query(SkillEmbedding).filter_by(
        skill_name=skill_name, 
        skill_version=skill_version
    ).first()
    
    if cached:
        return cached.embedding_vector
    
    # 2. 调用 embed_fn
    embedding = embed_fn(skill_text)
    
    # 3. 写入 DB
    db.add(SkillEmbedding(
        skill_name=skill_name,
        skill_version=skill_version,
        embedding_vector=embedding
    ))
    db.commit()
    
    return embedding
```

**并发安全**:
- `lru_cache` 是线程安全的
- DB 写入使用事务
- 读操作无锁

## 触发条件

**何时实施**:
- ✅ 技能数 >20
- ✅ 或 embedding API 调用成本明显
- ✅ 或 `ModernSkillSelector` 构造频率 >1/min

**当前状态**: 技能数 <20，无需优化

## 实施优先级

**优先级**: 低（Low）

**原因**:
1. 当前规模无感（<20 技能）
2. `SkillPipeline` 是长生命周期对象
3. 没有性能瓶颈报告

**监控指标**:
- 技能数量
- `ModernSkillSelector` 构造次数
- embedding API 调用次数/成本

## 监控实施

### 指标定义

```python
# 在 modern_selector.py 中添加
import time
from collections import defaultdict

class _SelectorMetrics:
    """Metrics for ModernSkillSelector performance monitoring."""
    
    def __init__(self):
        self.construction_count = 0
        self.construction_times = []
        self.embedding_calls = 0
        self.last_reset = time.time()
    
    def record_construction(self, duration_ms: float, skill_count: int):
        self.construction_count += 1
        self.construction_times.append(duration_ms)
        self.embedding_calls += skill_count
    
    def get_stats(self) -> dict:
        uptime = time.time() - self.last_reset
        return {
            "construction_count": self.construction_count,
            "construction_per_min": self.construction_count / (uptime / 60),
            "avg_construction_time_ms": sum(self.construction_times) / len(self.construction_times) if self.construction_times else 0,
            "total_embedding_calls": self.embedding_calls,
            "embedding_calls_per_min": self.embedding_calls / (uptime / 60),
        }

_METRICS = _SelectorMetrics()

class ModernSkillSelector:
    def __init__(self, ...):
        start = time.time()
        # ... existing code ...
        duration_ms = (time.time() - start) * 1000
        _METRICS.record_construction(duration_ms, len(self.rule_selector.skills))
```

### 监控端点

```python
# 在 API 中添加
@router.get("/metrics/selector")
def get_selector_metrics():
    """Get ModernSkillSelector performance metrics."""
    from core.skills.modern_selector import _METRICS
    return _METRICS.get_stats()
```

### 告警规则

```yaml
# 在监控系统中配置
alerts:
  - name: high_construction_frequency
    condition: construction_per_min > 1
    action: notify_team
    message: "ModernSkillSelector constructed >1/min, consider caching"
  
  - name: high_embedding_cost
    condition: embedding_calls_per_min > 50
    action: notify_team
    message: "Embedding API calls >50/min, cost may be significant"
  
  - name: skill_count_threshold
    condition: skill_count > 20
    action: notify_team
    message: "Skill count >20, consider implementing embedding cache"
```

### 日志记录

```python
# 在构造函数中添加
logger.info(
    "ModernSkillSelector constructed",
    extra={
        "skill_count": len(self.rule_selector.skills),
        "has_embed_fn": embed_fn is not None,
        "construction_time_ms": duration_ms,
    }
)
```

## 相关代码

**文件**: `core/skills/modern_selector.py`

**当前实现**:
```python
class ModernSkillSelector:
    def __init__(self, session: Session, llm_client=None, embed_fn=None):
        # ...
        self._index = SkillIndex(embed_fn)
        self._index.build(list(self.rule_selector.skills.values()))
        # ↑ 每次构造都重建索引，调用 N 次 embed_fn
```

**优化位置**:
```python
# 在 SkillIndex.build() 中添加缓存逻辑
def build(self, skills: list[SkillMetadata]) -> None:
    for skill in skills:
        cache_key = (skill.name, skill.version)
        if cache_key in cache:
            embedding = cache[cache_key]
        else:
            embedding = self._embed_fn(skill_text)
            cache[cache_key] = embedding
        self._entries.append((skill.name, embedding))
```

## 参考

- OpenAI embedding API: ~$0.0001/1K tokens
- 50 技能 × 100 tokens/技能 = 5K tokens = $0.0005/次构造
- 如果每分钟构造 1 次 = $0.72/天

## 决策

**当前决策**: 暂不实施，等技能数 >20 再评估

**下次评估**: 当技能数达到 20 或收到性能反馈时
