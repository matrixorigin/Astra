# Skill 层次结构与选择机制设计

## 问题

当前 skills 是扁平的，缺少：
1. **分类体系**：GitHub skills、Code skills、Analysis skills 混在一起
2. **选择机制**：不知道何时调用哪个 skill
3. **依赖关系**：skills 之间可能有依赖（如 code_review 依赖 get_pr_diff）

## 设计：三层 Skill 架构

### 1. Skill 分类层次

```
Skills
├── GitHub
│   ├── PR Management
│   │   ├── summarize_pr
│   │   ├── list_prs
│   │   └── code_review
│   ├── Issue Management
│   │   ├── analyze_bug
│   │   └── create_issue
│   └── CI/CD
│       └── ci_status
├── Code Analysis
│   ├── search_code
│   ├── generate_tests
│   └── analyze_complexity
└── Documentation
    ├── generate_docs
    └── update_readme
```

### 2. Skill 元数据扩展

```python
@dataclass
class SkillMetadata:
    """Extended skill metadata."""
    name: str
    version: str
    description: str
    
    # 新增：分类
    category: str  # "github" | "code" | "docs"
    subcategory: str  # "pr_management" | "issue_management"
    
    # 新增：触发条件
    triggers: List[str]  # 关键词触发
    # ["review", "PR", "pull request"] -> code_review
    
    # 新增：依赖
    dependencies: List[str]  # 依赖的其他 skills
    # code_review 依赖 get_pr_diff
    
    # 新增：优先级
    priority: int  # 1-10，数字越大优先级越高
    
    # 新增：成本
    cost_estimate: str  # "low" | "medium" | "high"
    # LLM-based skills 成本高
```

### 3. Skill 选择机制

#### 方案 A: 基于规则的选择（简单、可控）

```python
class SkillSelector:
    """Rule-based skill selector."""
    
    def select_skills(
        self,
        query: str,
        context: Context,
        max_skills: int = 3
    ) -> List[Skill]:
        """Select relevant skills based on query."""
        
        # 1. 关键词匹配
        candidates = self._match_by_keywords(query)
        
        # 2. 上下文相关性
        candidates = self._filter_by_context(candidates, context)
        
        # 3. 依赖解析
        candidates = self._resolve_dependencies(candidates)
        
        # 4. 优先级排序
        candidates = sorted(candidates, key=lambda s: s.priority, reverse=True)
        
        return candidates[:max_skills]
    
    def _match_by_keywords(self, query: str) -> List[Skill]:
        """Match skills by trigger keywords."""
        query_lower = query.lower()
        matched = []
        
        for skill in self.all_skills:
            if any(trigger in query_lower for trigger in skill.triggers):
                matched.append(skill)
        
        return matched
```

**示例**：
```python
query = "Review the PR #123"

# 匹配过程：
# 1. 关键词匹配: "review" + "PR" -> code_review, summarize_pr
# 2. 上下文: 有 repo_id -> 保留 GitHub skills
# 3. 依赖: code_review 需要 get_pr_diff -> 自动添加
# 4. 排序: code_review (priority=8) > summarize_pr (priority=6)
# 
# 结果: [code_review, get_pr_diff]
```

#### 方案 B: LLM-based 选择（智能、灵活）

```python
class LLMSkillSelector:
    """LLM-based skill selector."""
    
    def select_skills(
        self,
        query: str,
        available_skills: List[Skill]
    ) -> List[Skill]:
        """Use LLM to select relevant skills."""
        
        # 构建 prompt
        prompt = f"""
Given the user query and available skills, select the most relevant skills.

User Query: {query}

Available Skills:
{self._format_skills(available_skills)}

Return JSON array of skill names in order of relevance.
Example: ["code_review", "summarize_pr"]
"""
        
        # 调用 LLM
        response = self.llm.chat(prompt)
        selected_names = json.loads(response)
        
        # 返回 skill 对象
        return [s for s in available_skills if s.name in selected_names]
```

**优势**：
- 理解自然语言意图
- 处理复杂查询
- 适应新 skills

**劣势**：
- 增加延迟和成本
- 可能不稳定

#### 方案 C: 混合选择（推荐）⭐

```python
class HybridSkillSelector:
    """Hybrid skill selector: rules + LLM."""
    
    def select_skills(
        self,
        query: str,
        context: Context
    ) -> List[Skill]:
        """Hybrid selection strategy."""
        
        # 1. 快速规则匹配（过滤）
        candidates = self._rule_based_filter(query)
        
        if len(candidates) <= 3:
            # 候选少，直接返回
            return candidates
        
        # 2. LLM 精选（排序）
        if len(candidates) > 3:
            # 候选多，用 LLM 排序
            return self._llm_rank(query, candidates)[:3]
        
        return candidates
```

**优势**：
- 规则快速过滤（低成本）
- LLM 精确排序（高质量）
- 平衡性能和准确性

### 4. Skill 执行流程

```python
class SkillOrchestrator:
    """Orchestrate skill execution."""
    
    async def execute_query(
        self,
        query: str,
        session_id: str
    ) -> Dict[str, Any]:
        """Execute query with skill selection."""
        
        # 1. 构建 context
        context = self.context_mgr.build_context(session_id, query)
        
        # 2. 选择 skills
        selected_skills = self.selector.select_skills(query, context)
        
        logger.info(f"Selected skills: {[s.name for s in selected_skills]}")
        
        # 3. 解析依赖
        execution_plan = self._build_execution_plan(selected_skills)
        
        # 4. 执行 skills
        results = {}
        for skill in execution_plan:
            try:
                result = await skill.execute(self._extract_input(query, skill))
                results[skill.name] = result
            except Exception as e:
                logger.error(f"Skill {skill.name} failed: {e}")
                results[skill.name] = {"error": str(e)}
        
        # 5. 合并结果到 context
        enhanced_context = self._merge_results(context, results)
        
        # 6. 调用 LLM 生成最终响应
        response = self.llm.chat(enhanced_context.to_prompt())
        
        return {
            "response": response,
            "skills_used": [s.name for s in selected_skills],
            "skill_results": results
        }
```

### 5. Schema 更新

```sql
-- 扩展 skills_registry 表
ALTER TABLE skills_registry ADD COLUMN category VARCHAR(50);
ALTER TABLE skills_registry ADD COLUMN subcategory VARCHAR(50);
ALTER TABLE skills_registry ADD COLUMN triggers JSON;
ALTER TABLE skills_registry ADD COLUMN dependencies JSON;
ALTER TABLE skills_registry ADD COLUMN priority INT DEFAULT 5;
ALTER TABLE skills_registry ADD COLUMN cost_estimate VARCHAR(20);

-- 创建 skill_categories 表
CREATE TABLE skill_categories (
    category_id VARCHAR(36) PRIMARY KEY,
    name VARCHAR(100) NOT NULL,
    parent_category_id VARCHAR(36),
    description TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    
    INDEX idx_parent (parent_category_id)
);

-- 创建 skill_executions 表（记录 skill 使用）
CREATE TABLE skill_executions (
    execution_id VARCHAR(36) PRIMARY KEY,
    session_id VARCHAR(36) NOT NULL,
    event_id VARCHAR(36),
    skill_name VARCHAR(100) NOT NULL,
    skill_version VARCHAR(20) NOT NULL,
    input_data JSON,
    output_data JSON,
    status VARCHAR(20),  -- success | failed
    error_message TEXT,
    execution_time_ms INT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    
    INDEX idx_session (session_id),
    INDEX idx_skill (skill_name, skill_version),
    INDEX idx_created (created_at)
);
```

### 6. 配置示例

```yaml
# skills.yaml
skills:
  - name: code_review
    category: github
    subcategory: pr_management
    triggers:
      - review
      - PR
      - pull request
      - code review
    dependencies:
      - get_pr_diff
    priority: 8
    cost_estimate: medium
    
  - name: summarize_pr
    category: github
    subcategory: pr_management
    triggers:
      - summarize
      - summary
      - PR
    dependencies: []
    priority: 6
    cost_estimate: low
    
  - name: analyze_bug
    category: github
    subcategory: issue_management
    triggers:
      - bug
      - issue
      - error
      - analyze
    dependencies:
      - search_code
    priority: 7
    cost_estimate: high
```

## 决策流程示例

### 场景 1: "Review PR #123"

```
1. 关键词匹配:
   - "review" -> [code_review, ...]
   - "PR" -> [code_review, summarize_pr, list_prs]
   
2. 交集: [code_review]

3. 依赖解析:
   - code_review 依赖 get_pr_diff
   - 添加 get_pr_diff
   
4. 执行计划:
   - get_pr_diff(repo_id, pr_number=123)
   - code_review(repo_id, pr_number=123)
   
5. 结果合并到 context，LLM 生成最终响应
```

### 场景 2: "Find bugs in authentication code"

```
1. 关键词匹配:
   - "bugs" -> [analyze_bug]
   - "authentication" -> [search_code]
   
2. 候选: [analyze_bug, search_code]

3. LLM 排序（因为候选 > 1）:
   - search_code (先找代码)
   - analyze_bug (再分析)
   
4. 执行计划:
   - search_code(query="authentication")
   - analyze_bug(files=search_results)
```

## 实现优先级

### Phase 1 (MVP) ✅ - 已完成
- 基于规则的选择
- 关键词匹配
- 简单依赖解析
- 固定优先级

### Phase 2 (Current) ⭐ - 正在实现
**Native Function Calling Integration**
- ✅ ModernSkillSelector: 使用 LLM 原生 function calling
- ✅ 粗筛（Retrieval）: 规则匹配选出 Top-K 候选
- ✅ 精调（Native Execution）: LLM 直接输出函数调用+参数
- ✅ 一步到位：`code_review(pr_id=123, focus="performance")`

**Model Routing (Mixture of Agents)**
- ✅ ModelRouter: 根据 skill category 路由到不同模型
- ✅ Code skills → DeepSeek Coder
- ✅ GitHub skills → GPT-4
- ✅ Docs skills → Claude 3 Sonnet
- ✅ 优先级/成本自适应调整

**效果对比**：
```python
# 旧方案（Phase 1）
LLM 输出: ["code_review"]
代码再问: "你要 review 哪个 PR？"
用户输入: "PR #123"
执行: code_review(pr_number=123)

# 新方案（Phase 2）⭐
LLM 直接输出: code_review(pr_id="repo/123", pr_number=123, focus="performance")
一步到位！
```

### Phase 3 (Future) - 智能选择
   - 学习用户偏好
   - 自动调整优先级
   - 成本优化

## 总结

**当前状态**: 扁平 skills，无选择机制

**建议方案**: 
1. ✅ 添加 skill 分类和元数据
2. ✅ 实现混合选择器（规则 + LLM）
3. ✅ 记录 skill 执行历史
4. ✅ 支持依赖解析

**下一步**: 实现 SkillSelector 和 SkillOrchestrator
