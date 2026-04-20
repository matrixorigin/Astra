# Tool / Skill 选择：TF-IDF + Embedding 混合与可进化配置

> **Status**: Planned — not implemented  
> **Last updated**: 2026-04-21

## 修订记录

| 日期 | 变更 |
|------|------|
| 2026-04-21 | Review：澄清 CLI/云侧边界、与现有配置键对齐、区分多种「置信度」、补充技能侧 `SkillSearchSettings`、嵌入构造/观测/非目标/验收标准 |
| 2026-04-20 | 初稿 |

---

## 1. 目标

- **减少主 LLM 上下文中的 tool / skill 相关 token**：主要是 **动态 function tool 的 JSON schema 块**；技能侧多为 **元数据列表 + 激活后的说明文本**，同样随候选数量膨胀。
- **保留可用性与确定性默认路径**：embedding 限流、超时或故障时 **退化为纯 TF-IDF（工具）+ 现有启发式（技能）**，不阻塞回合。
- **参数化 + 闭环可调**：行为由 **runtime / 会话 / 用户 profile** 可解释地驱动；学习回路只动 **允许清单内的标量**，避免不可审计的漂移。

## 2. 背景（当前实现）

### 2.1 工具路径

- **打分与索引**：[`tool_registry/scoring.rs`](../../rust/crates/runtime/src/tool_registry/scoring.rs) — TF-IDF 余弦（预计算 IDF）+ **trigger 词边界匹配** + 路由/质量等；静态元数据见 [`tool_registry_meta.rs`](../../rust/crates/astra-turn-core/src/tool_registry_meta.rs)（`description` 字段注释预留「embedding 索引」用语，当前产品仍以 **多语言 triggers** 为主）。
- **选择器**：[`TfIdfSelector::select`](../../rust/crates/runtime/src/tool_selector.rs) — `RoutingEngine`、实体/pattern boost、共现、`ToolRegistry::select_routed_with_pressure`；返回 `SelectionResult.confidence`（**约** `routing.confidence * 0.7 + tool_factor`，与动态工具数量有关）—— **这不是** TOML 里的 `tool_selection.confidence_threshold`（后者用于别处配置语义，勿混用）。
- **生产 CLI**：[`repl_runtime.rs`](../../rust/crates/astra-cli/src/cli/repl_runtime.rs) 仅装配 **`TfIdfSelector`**，已移除每轮 **`LlmToolSelector`** 预选（避免二次 LLM 往返延迟）。
- **库内保留**：[`LlmToolSelector`](../../rust/crates/runtime/src/tool_selector.rs)、[`FallbackSelector`](../../rust/crates/runtime/src/tool_selector.rs) — `FallbackSelector` 的行为是 **先跑 fallback 链上的 TfIdf 结果**，仅在低置信等条件下再调用 primary（曾为 LLM）。**「高置信则跳过后续重活」** 的门控可直接类比到 **「高置信则跳过 embedding」**。

### 2.2 技能路径（与工具独立）

- [`skill_tool.rs`](../../rust/crates/runtime/src/turn/skill_tool.rs)：`select_skills_for_turn`（token / 触发词 / 别名 / 质量加权 + 阈值）、`discover_skills`、`visible_skills_for_host_turn`。
- **会话/载荷侧设置**：[`SkillSearchSettings`](../../rust/crates/core/src/config.rs)（`astra_core`）— `dynamic_surface`、`min_catalog_size`、`surface_cap`（默认 cap 14）等，由 WS/API 载荷等传入（见 [`ws_handler.rs`](../../rust/crates/runtime/src/server/ws_handler.rs) 等）。**向量辅助若落地，应与这些开关协同**（例如在 `dynamic_surface=true` 时 rerank 可见子集）。

### 2.3 配置与 per-user 已有挂钩

- **工具选择**：[`ToolSelectionConfig`](../../rust/crates/astra-config/src/runtime_config.rs) — `max_tools`、`confidence_threshold`、`tool_budget_tokens`、`max_tool_schema_tokens`、`use_learned_patterns` 等；`selector_model` **遗留且无效果**（文档见源码注释）。
- **Profile / AB 已能改的键**（实现闭环时的挂载点）：[`user_profile.rs`](../../rust/crates/astra-config/src/user_profile.rs)、[`ab_testing.rs`](../../rust/crates/astra-config/src/ab_testing.rs) 已支持例如 `tool_selection.confidence_threshold`、`tool_selection.max_tools`、`tool_selection.tool_budget_tokens`。**Embedding 相关标量建议同样走这类键或扩展 `ToolSelectionConfig` 子结构**，便于审计与 shadow。

### 2.4 范围说明（避免过度推广）

- 本文 **不假设** 云侧 `astra-server` 与 CLI 共用同一套「本地装配的 `Box<dyn ToolSelector>`」；云路径可能通过 **thin client / payload** 携带已裁剪工具列表。落地时需 **分别标注**：embedding 是在 **edge 组 payload 前** 还是 **cloud 组 payload 前** 执行，以及 **向量索引与 query 是否同区域**（延迟）。

---

## 3. 策略草案

### 3.1 默认：级联（推荐）

1. **工具**：跑现有 TF-IDF + 路由管线 → 候选集 + `SelectionResult.confidence`；可选增加 **主指标**：如 top 动态工具的 **score margin**（第一名与第二名综合分之差），与 routing 置信 **并联** 作为门控，减少单一标量误判。
2. **门控**：若 `confidence ≥ τ_high` 且（可选）`margin ≥ m_min` → **跳过 embedding**，维持当前排序与 budget 截断。
3. 否则 → **每用户 query 一次 embedding**（或对缓存过的 query 向量复用），与 **预计算的 tool/skill 文档向量** 做 **cosine rerank**；或对 TF-IDF 候选 **扩召回**（从全目录取向量 top-K 再与 TF-IDF 并集后截断）。
4. **技能**：在 `select_skills_for_turn` 输出上 **二次排序** 或 **替换 weak 分支的填充顺序**，须保持 **`surface_cap`** 与 pinned / discovered 语义不变。

### 3.2 可选：并行融合

- TF-IDF top-K₁ ∪ 向量 top-K₂ → RRF 或加权分；召回更稳，**QPS、尾延迟与索引体积**更高，适合 P2+。

### 3.3 低置信与产品兜底

- 与 [`ConfidenceFallback::Broaden`](../../rust/crates/runtime/src/tool_selector.rs) 等已有语义对齐：扩大工具预算、降低 budget pressure、依赖 boost_terms / domain hints。
- 极低置信：**澄清提示**、允许 **`discover_skills`** 路径；**不**默认恢复「整轮 LLM 仅选工具名」除非 **硬超时 + 可观测证明** 不会重现历史 10–80s 问题。

---

## 4. Token 与成本（客观表述）

| 项 | 说明 |
|----|------|
| **可能下降** | 动态 **OpenAI-style function schemas** 随候选数近似线性膨胀；收窄候选 → 通常 **prompt token 下降**。 |
| **不一定下降** | 系统提示、历史、memory、大 tool result **不受**本方案直接影响；若模型 **反复 discover** 补工具，**总 token 可能反弹**。 |
| **Embedding 成本** | 在「快且便宜」前提下，常见模式是 **每轮 1 次 query 向量** + 本地向量运算；须对比 **省下的 schema token 成本**（线上 p50/p95）。 |
| **技能** | 减少的是 **列表/摘要** 与后续 **skill 正文** 的触发概率相关的间接成本，需单独度量。 |

---

## 5. 文档向量与索引（设计约束）

### 5.1 文本构造（建议）

- **工具**：与 TF-IDF 文档对齐 — `name + description + triggers`（可截断到固定字符上限并哈希版本）。
- **技能**：`name + 描述 + 触发词/aliases + 可选 tags`（来自 SKILL 元数据）；**动态技能**需 **版本或 mtime** 参与缓存键。
- **Query**：用户本轮消息（可拼接最近一轮 tool 名等极短上下文 — 谨慎控制长度）。

### 5.2 模型与一致性

- **语料与 query 必须使用同一 embedding 模型与维度**；模型升级需 **全量或增量重索引** + `embed_index_version`。

### 5.3 存储（若用 MatrixOne 向量）

- **距离函数**：索引定义与查询必须一致（如 cosine ↔ cosine），见仓库 DB 规范。
- **写入**：批量插入；避免依赖 JSON 列上的过滤做热路径。

### 5.4 隐私与安全

- 记录 raw query 进日志/审计时遵守 **数据最小化**；embedding 服务若第三方，需明确 **留存与区域**。

---

## 6. Runtime 参数化（设计锚点 → 实现时落入 `ToolSelectionConfig` 或子表）

| 参数 | 含义 | 备注 |
|------|------|------|
| `embedding_tool_enabled` | 工具侧向量辅助总开关 | 默认 false 直至 P1 验证完成 |
| `embedding_skill_enabled` | 技能侧向量辅助 | 独立 rollout |
| `embedding_skip_min_confidence` | 高于则跳过 embedding | 勿与 `tool_selection.confidence_threshold` 混名 |
| `embedding_skip_min_score_margin` | 综合分 top1−top2 下限 | 可选 |
| `embedding_rerank_pool_k` | 进入向量阶段的候选上限 | 控延迟 |
| `embedding_vector_top_k` | 扩召回时向量检索 K | |
| `embedding_timeout_ms` | 单次 RPC 超时 | 超时 → 无 embedding |
| `embedding_max_inflight` / QPS | 客户端限流 | |
| `embedding_model_id` / `embed_index_version` | 模型与索引世代 | 与 catalog hash 联动 |
| `embedding_fusion_weight_tfidf` / `_emb` | 融合权重 | 可选 |

**现有键（保持兼容）**：`max_tools`、`tool_budget_tokens`、`max_tool_schema_tokens` 仍约束 **最终进入 payload 的上限**；embedding 不应绕过这些预算 unless 显式配置允许。

---

## 7. 弹性与退化

- **优先级**：正确完成回合 **优于** 使用 embedding。
- **触发退化**：超时、5xx、空向量、限流命中、索引版本不匹配 → **跳过 embedding**，使用当前 TF-IDF / 技能启发式结果。
- **观测**：结构化字段建议包含 `embedding_branch`（`skipped_ok` | `rerank` | `recall_expand` | `degraded_timeout` | `degraded_rate_limit`）、`embedding_latency_ms`、`candidate_count_before` / `after`。

---

## 8. 可进化与 per-user 闭环

- **可动参数（建议白名单）**：`embedding_skip_min_confidence`、`embedding_rerank_pool_k`、`fusion` 权重、超时 — **不动** pinned 工具集与 `max_tools` 硬上限安全区。
- **已有基础设施**：`user_profile` / `ab_testing` 对 `tool_selection.*` 的补丁；闭环可写入 **影子配置** 再对比 **工具错误率、discover 次数、每轮 prompt tokens**。
- **冷启动**：新用户 / 新会话 → 全局默认 + **更保守的跳过阈值**（多走 embedding 或更宽候选，直到有统计）。
- **防振荡**：指数滑动平均、变更冷却、单用户单日最大步长；异常自动回滚默认。

---

## 9. 风险与非目标

### 9.1 风险摘要

| 风险 | 缓解 |
|------|------|
| 候选过窄漏工具 | pinned + 路由领域包 + 低置信 Broaden |
| 与 `confidence_threshold` 语义混淆 | 配置命名区分 `embedding_skip_*` vs registry 阈值 |
| 闭环把阈值推坏 | 白名单 + 边界 + 回归看板 |
| 模型/索引不一致 | 强校验 `embed_index_version` |
| 跨语言仍失效 | 保留 triggers；bad-case 进触发词或微调语料 |

### 9.2 非目标（本文档不承诺）

- **完全取代** TF-IDF 与 trigger 维护（小目录下其可解释性仍是资产）。
- **无上限** 自动收紧候选（必须受 `max_tools` / `surface_cap` 与安全下限约束）。
- 在 **未证明延迟** 前恢复 **默认** LLM 工具预选。

---

## 10. 实施阶段与验收（建议）

| 阶段 | 交付 | 验收要点 |
|------|------|----------|
| **P0** | 配置占位 + 全路径退化（feature off = 当前行为） | 无回归；压测限流 |
| **P1** | 工具静态目录离线向量 + 级联 rerank | p95 延迟预算内；漏召率 / 错误工具率不劣于基线；prompt token p50 下降可量化 |
| **P2** | 技能 surface/discover 辅助 + 可选融合 | `surface_cap` 行为不变；discover 次数不异常上升 |
| **P3** | 与 ProgressiveCalibrator / profile 闭环 | 仅白名单参数；可回滚；审计日志完整 |

---

## 11. 参考文档与代码

- [Skills and Tools](skills-and-tools.md)
- [Evaluation and Evolution](evaluation-and-evolution.md)
- [Context Window Management](context-window-management.md)
- [session-observability.md](session-observability.md)（工具选择/决策审计扩展字段时可对照）
- [multi-agent-cloud-runtime.md](multi-agent-cloud-runtime.md)
