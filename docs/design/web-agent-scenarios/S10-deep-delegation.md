# S10 — 深层 delegation：安全团队做年度供应链漏洞扫描报告

> Status: Draft v0.1
> DAUT: **A4, A13, A15**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：某 800 人科技公司 CISO（安全负责人）。习惯用"清单 ×
证据 × 审计链"方式推进工作，对 agent 最在意三件事：报告里每条漏洞
能回溯到扫描任务、不想被无关误报淹没、中途要能直接纠正某一路执行
分支而不整张表重跑。他不写 prompt，只下命令。

**业务背景**：公司代码资产分散在 50+ 仓库（monorepo 未完成迁移），
Q1 年度供应链漏洞扫描要一次性覆盖 SAST（语义静态分析）、SCA（依赖
组件审计）、secret scan（硬编码凭证）三类引擎。单仓库扫描 15–40
分钟，raw 输出平均每仓 3–20 MB（含误报），必须经过 reviewer-agent
triage 才能见人。传统做法是跑 CI + 人工汇总两周，现在要求 orchestrator
agent 在 1 个 L0 会话里端到端跑完。

**一句话目标**：从一句"给我 Q1 全量漏洞扫描报告"起，通过 5 层 delegation
树自动完成 50+ 仓库的多引擎扫描 + 误报过滤 + 严重性聚合，最终
产出一份**只含 severity ≥ high 的真阳漏洞清单**，过程中 L0 manifest
始终线性增长不吃 L3/L4 原始数据。

**层级图**：

```
L0  user session (sess-vuln-annual-q1)   ← 安全负责人发起
└─ L1  orchestrator-agent run            ← parent_run_id=NULL
   └─ L2  planner-agent run              ← parent_run_id=L1, delegation_id=dl-plan
      └─ L3  executor-agent run × 4      ← 并行；每个负责 ~12 仓库
         └─ L4  reviewer-agent run × 4   ← 每个 executor 内部派一个
```

**关键数值指标**：

| 指标 | 预计值 |
| --- | --- |
| 层级深度 | 5（L0 用户 session → L4 reviewer） |
| 独立 `agent_runs` 行 | 约 15（含 T8 retry 子树 +2、T10 reporter 子 run） |
| 仓库总数 | 52（executor-1 上 13、executor-2 上 13、executor-3 上 13、executor-4 上 13） |
| 工具调用总数 | ~680（平均每仓约 13 次：SAST 1 + SCA 1 + secret 1 + 若干 triage） |
| raw artifact 数 | ~160（每仓 3 份 raw output），经 reviewer 归并后留存摘要 ~60 |
| 最终报告漏洞条数 | ~30（severity ≥ high 真阳；从原始 ~3800 条压缩过来） |
| 用户回合数 | 10（T1..T10，都发生在 L0） |
| delegation tree 节点 | 15 个 run + 11 条 `delegation_state` row |
| bubble-up 事件 | ≥ 1（T7，critical finding 从 L4 到 L0 链式更新） |
| 跨层纠偏 | 1 次（T8，L0 层重派 L3.executor-2 子树） |

## 2. 被测 DAUT

本场景把"child agents = first-class sessions / run children / both"
这条 open question（§Open Questions）放大到 5 层，逼迫 A13 给出
可工程化的答案；同时要求 A4 在 5 层 manifest 里每层只看自己本级
摘要，A15 在深层 delegation 下正确识别"重派某子节点"而非"新任务"。

| 编号 | 压测方式 |
| --- | --- |
| **A4** | L0 的 `context_manifests` 在 T4/T7/T10 三个关键回合都**绝不**能把 L3 executor 或 L4 reviewer 的 raw transcript / raw tool_output 纳入任何 zone。L0 `retrieved_facts` zone 只允许承载 `session_state_items(category='delegation_state')` 的递归摘要（每层 one-line）+ `category='summary'` 回写。每层自己的 manifest 独立持久化，`context_manifests.run_id` 必须分别指向 L1/L2/L3/L4 的 run_id，`reason` 分别标记 `delegation_plan` / `delegation_dispatch` / `delegation_execute` / `delegation_triage`。任何把跨层内容"拍平"进单一 manifest 的实现都违反 A4。 |
| **A13** | 5 层 `parent_run_id` 链必须可通过单次递归 CTE 或预计算 `ancestor_path` 列回溯到 L0 用户 session。`agent_runs` 15 行全部写齐 `parent_run_id / delegation_id`；`session_state_items(category='delegation_state')` 作为**父 session 对子 run 的投影指针**，与 `agent_runs.parent_run_id`（run 级别父子边）形成**双表冗余但同义**的关系——这要求设计文档明确两者各自的权威范围。T8 retry 要求 `agent_runs.retry_of` 字段在深层 delegation 下的语义（重做单节点 / 整棵子树 / 重派兄弟）有确定的落地方式。 |
| **A15** | T8 用户原话："把 executor-2 的数据库扫描重做一遍。"这句话里 "executor-2" 与 "数据库扫描" 是**已存在的 L3 run + L3 run 下某个 SAST scope** 的结构化引用，不是新任务。意图抽取必须走 §Next-Action Extraction Cost 里的分级：先走 structured event（L3 run_id 是否能从上文 delegation tree 直接匹配）→ rule（关键词 "重做 / redo / rerun" + 已有 executor 编号）→ 小模型兜底。正确路径**严禁**把这句自然语言丢给主 LLM 让它生成新 `session_todos`；正确结果是向 L3.executor-2 所在 run 发 `retry_of` 事件 + 可能级联重派其 L4 reviewer。 |

## 3. 会话时间线

10 个回合全部发生在 L0 用户视角；每回合统一列出：**用户原话**、
**涉及层级的 state 变化**、**各层 context_manifest zone 分配对比**、
**LLM 看不到什么**。零号约定：`L0` 不跑 LLM，只收用户输入 +
渲染界面，真正产 manifest 的是 L1 及以下。

### T1 · 安全负责人发起年度扫描（L0 会话建立）

**用户原话**：「启动 Q1 年度供应链漏洞扫描。范围：所有 production
仓库（tier-1 + tier-2）。要求：SAST + SCA + secret scan 三类全跑，
reviewer 过一遍误报，最终给我一份只含 high 与 critical 真阳的
清单，按仓库分组、附最小复现信息。」

**涉及层级**：L0 新建 `agent_sessions` 行 `sess-vuln-annual-q1`，
写入 `session_state_items(category='anchor', item_key='mission',
payload_json={scope:'tier1+tier2', engines:['SAST','SCA','secret'],
severity_filter:'>=high'})`。L1 orchestrator-agent 启动：新建
`agent_runs` row `run-L1-orch`，`parent_run_id=NULL`、
`delegation_id=NULL`、`trigger_type='user_message'`、
`status='running'`。L0 尚无 delegation_state（要等 L1 派 L2 才写）。

**manifest zone**（L1 orch 本级）：`system_static` 1200 /
`tool_schemas` 1100（仅 delegation-dispatch + state-query 工具，
不含任何扫描引擎工具——扫描工具挂在 L3）/ `skills` 600
（`security-orchestration` skill）/ `session_anchor` 450（含 CISO
的 mission 原文 + severity_filter）/ `plan_todo` 0（尚未规划） /
`recent_tail` 300（用户原话 1 条）/ `workspace` 200。总 ≈ 3850，
`reason='delegation_kickoff'`。

**LLM 看不到**：没有 L2/L3/L4（都还不存在）；更关键的是 L1 不直接
持有任何扫描引擎 tool schema——schema 会在下一步 delegation 时按
子 agent 的能力签名挂到各自 run 的 manifest。

### T2 · orchestrator(L1) 拆顶层任务并派 planner(L2)

**动作**：L1 决定分两步推进：先做"仓库清单 + 风险排序"，再分发
执行。于是派 L2 planner。L1 写入 1 条
`session_state_items(category='delegation_state', item_key='plan')`，
payload_json = `{child_run_id:'run-L2-plan', delegation_id:'dl-plan',
scope:'repo-inventory+risk-ranking', expected_output:
'artifact:repo-groups-by-priority', status:'active'}`。同时
`agent_runs` 写入 L2 row：`parent_run_id='run-L1-orch'`、
`delegation_id='dl-plan'`、`trigger_type='delegation'`。L1 当前
`plan_todo` zone 折叠为 "⇣ delegated → run-L2-plan" 单行。

**manifest zone 对比**：
- L1 本级：`session_anchor` 450 / `plan_todo` 200（一行 delegated 指针）/
  `delegation_state` 180 / `recent_tail` 600 / 总 ≈ 4500。
- L2 本级新建：`system_static` 1200 / `tool_schemas` 800（只挂 repo
  元数据查询 + CMDB 工具，不挂扫描引擎）/ `session_anchor` 500
  （从 L1 handoff 过来的 mission + scope）/ `recent_tail` 0 /
  总 ≈ 3200，`reason='delegation_plan'`。
- L0 用户界面：只看到一条 "orchestrator 正在分派 planner"。

**LLM 看不到**：L1 不看 L2 的内部推理与工具 schema；L2 不看 L1 的
recent_tail 里用户原话以外的 meta 讨论；L0 永远只看到各层
delegation_state 的 one-line。

### T3 · planner(L2) 产出分组计划回写 L1

**动作**：L2 查 CMDB 得 52 个仓库；按（tier + 语言栈 + 近 90 天
commit 量）聚类成 4 组，产 artifact `repo-groups-by-priority.json`。
L2 向父 L1 回 handoff：更新 L1 的 `session_state_items` row
`category='delegation_state', item_key='plan'` 的 payload_json
追加 `{child_summary_text:'4 组 × ~13 仓库；tier-1 优先；
recommend_parallel_executors:4', child_artifact_refs:
['artifact:repo-groups-by-priority.json'], last_handoff_at:<ts>,
status:'progress'}`。L2 自己的 `session_state_items(category=
'summary')` 写入 version=1，token≈350。L2 run `status='completed'`。

**manifest zone 对比**：
- L1（下一轮被唤醒）：`delegation_state` 450（多了 child_summary
  350 + artifact ref）/ `plan_todo` 800（L1 拿 artifact 展开成 4 组
  待派执行）/ `session_anchor` 450 / 总 ≈ 5100，`reason=
  'delegation_resume'`。
- L2 本级：`summary` 350 / `tool_previews` 200（artifact preview_text
  只含 4 组标题 + 仓库数，原文走 artifact_ref）/ 总 ≈ 4200。
- L0：用户此刻如果打开界面，看到 "planner 产出 4 组计划"，
  点击才加载 artifact 内容（走 `session_artifacts` 独立查询）。

**LLM 看不到**：L1 不看 L2 的 CMDB 查询原始 JSON（只拿 summary +
artifact ref）；L2 的工具调用 trace 留在 `agent_run_events` 里可审计
重放，但不进任何 prompt。

### T4 · L1 派 4 个并行 executor(L3)

**动作**：L1 按分组结果派 4 个 executor。`agent_runs` 新增 4 行：
`run-L3-exec-{1..4}`，全部 `parent_run_id='run-L1-orch'`、
`delegation_id='dl-exec-{1..4}'`、`trigger_type='delegation'`、
`execution_mode='cloud'`、`status='running'`。L1 在
`session_state_items` 再写 4 条 `delegation_state` row
（item_key=`exec-1 .. exec-4`），每条 payload_json 含
`{child_run_id, scope_section:'group-N', repo_count:13,
expected_output:'artifact:scan-report-group-N', status:'active'}`。

**manifest zone 对比**（L1 这轮最大）：
- L1：`delegation_state` 800（4 条 child summary 各 ~150 token，
  含 child_run_id + scope + repo_count，**不含任何 per-repo 数据**）
  / `plan_todo` 500（4 行 delegated 指针）/ `recent_tail` 700 /
  `session_anchor` 450 / `summary` 350（L2 的 summary 已 roll-up）
  / 总 ≈ 5700。
- L3 各 executor 本级：`system_static` 1200 / `tool_schemas` 2400
  （SAST + SCA + secret-scan 三类扫描工具全挂）/ `session_anchor` 450
  （mission + 自己分到的 13 个仓库列表）/ `plan_todo` 800（13
  仓库排期）/ 总 ≈ 5000，`reason='delegation_execute'`。
- L0 用户界面：显示 4 个并行 executor 的进度条，每条上显示当前
  仓库名 + 已完成数。

**LLM 看不到**：L1 不看任何 per-repo 扫描策略；L3 彼此不看兄弟
executor 的进度（隔离，防止互相吞 token）；L4 reviewer 尚未派出。

### T5 · executor-2(L3) 派 reviewer(L4) 处理 raw SAST 输出

**动作**：executor-2 在扫完前 3 个仓库后，累计 raw SAST 输出
约 12 MB（~1200 条 finding，估计 80% 误报）。executor-2 决定
**不自己 triage**，派 reviewer。`agent_runs` 新增 `run-L4-rev-2`，
`parent_run_id='run-L3-exec-2'`、`delegation_id='dl-review-2'`。
executor-2 写 `session_state_items(category='delegation_state',
item_key='review-batch-1')`，payload_json 含 `{child_run_id,
input_artifact_refs:['artifact:sast-raw-exec2-batch1.jsonl'],
expected_output:'artifact:sast-triaged-exec2-batch1',
dedup_policy:'by-file+rule-id', status:'active'}`。

**manifest zone 对比**：
- L3 exec-2 本级：`delegation_state` 180（单条 reviewer 指针）/
  `tool_previews` 300（raw SAST 文件的 preview，只含前 5 行 + 行数
  统计，**不进 raw 全文**）/ `plan_todo` 800 / 总 ≈ 4800。
- L4 reviewer-2 本级：`system_static` 1200 / `tool_schemas` 600
  （只挂 triage rules 查询 + artifact read + 去重工具）/
  `session_anchor` 500（mission + dedup_policy）/
  `retrieved_facts` 400（历史已知误报规则库的 top-20）/
  总 ≈ 3600，`reason='delegation_triage'`。
- L1：**不感知**此层派发（L1 的 `delegation_state` 行仍保留 exec-2
  为 `status=active` 但不展开 child 的 child）；L0 用户界面
  看到"executor-2 正在与 reviewer 协作"的一行状态文字（来自 L3
  到 L0 的**两跳** delegation_state 递归渲染，但只递归 1 层深度；
  超过 1 层的递归摘要合并成 "(+1 nested review)" 占位）。

**LLM 看不到**：L1 的 manifest 里 `delegation_state` zone 不膨胀
（L1 看到的仍是 "exec-2: running / 3 of 13 repos done"，不看
 reviewer 存在）；L4 reviewer 拿到的是 artifact_ref 不是 raw 正文
——raw 正文仍在 `session_tool_outputs` + S3/artifact store，按
§9 §Tool Result and Artifact References 规则走 preview_text。

### T6 · 用户在 L0 层追问"仓库 `payments-service` 扫描到哪了"

**用户原话**：「`payments-service` 是 tier-1 关键仓库，我现在就想
看它扫到哪一步了，别给我整体进度条。」

**动作**：L0 接到请求后**不重启 L1 run**，而是发起一次只读的
**递归 delegation_state 查询**：按 `item_key LIKE '%payments-
service%'` 反向找 artifact_ref；未命中时，按 `delegation_state.
payload_json -> '$.scope_section'` 的仓库列表（已按 repo 名
索引成独立 `session_state_items(category='delegation_state',
item_key='exec-N:repo:payments-service')`——见 §8 建议）定位到
`run-L3-exec-2`。然后逐层（L3 → L4）读各自的 `category='summary'`
最新 row。返回：「exec-2 正在 triage batch-1，payments-service
已完成 SAST + SCA，secret scan 排队中，已发现 2 条 medium、待
reviewer 复核。」

**manifest zone**：本次查询**不过 LLM**，走 §Normal Query
Budget 的 3 次索引查询：一次按 item_key 定位、一次查 L3
summary、一次查 L4 in-flight triage state。L0 UI 直出。仅当
用户追问"给我细节"时，才会拉起 L1 的一轮新 manifest（reason=
`user_drill_down`）把定位结果作为 `retrieved_facts` zone 注入。

**LLM 看不到**：所有层的 transcript；raw artifact。这一回合**完全
不走 LLM**，在设计文档 §History Reconstruction Modes 的 "Runtime
resume: Bounded materialization" 与 "Web display history" 之间——
**S10 揭示需要第 4 种模式："Delegation tree drill-down"**（见 §8）。

### T7 · executor-3 发现 critical 漏洞，触发 bubble-up

**动作**：exec-3 所在组里 `auth-gateway` 仓库的 reviewer-3 确认一条
severity=critical 的 RCE（Spring4Shell 变种，已触发 tier-1）。按设计，
reviewer 只负责写自己层级的 `session_state_items`，但 critical 必须
**跨层 bubble-up** 到 L0。流程（本场景建议落地为一等机制）：

1. L4 reviewer-3 写 `session_state_item_events` 行
   `mutation='bubble_up'`（新 mutation 类型，见 §8 反馈）、
   `item_id=<critical finding row>`、`payload_json=
   {severity:'critical', repo:'auth-gateway', rule:'CVE-2022-22965-like',
    bubble_target_scope:'root_session'}`。
2. 该事件触发 parent L3 exec-3 的 delegation_state payload patch
   （追加 `critical_findings_count:+1` + `bubble_ref`）+ 进一步
   向 L2/L1 逐层写同 item_key 的递归投影。
3. L1 收到后立刻向 L0 `session_state_items(category='anchor',
   item_key='alerts')` 插 1 条，`priority=0`（最高）。L0 UI 顶部
   弹出红色 banner，附 artifact_ref 指向复现包。

**整个 bubble 链不重启任何 LLM 调用**——纯数据库投影事件；
链路延迟 = 逐层写 `session_state_item_events` 的累加时间
（理想 < 500 ms）。

**manifest zone**：本回合**不产生新的 LLM manifest**。只在 L0
下一次渲染时，`session_anchor` zone 多一条 `alerts[0]=<critical>`，
`priority` 排序保证它在 anchor zone 最上端。

**LLM 看不到**：bubble 过程中间层的 raw finding JSON（仍在 L4
的 artifact store）；L0 只看到 severity + repo + rule_id +
artifact_ref，不看 finding 全文。

### T8 · 用户跨层纠偏："executor-2 的数据库扫描重做一遍"

**用户原话**：「executor-2 的数据库相关仓库那批 SAST 我看规则集走
偏了，漏了 SQL 注入的几个变种。把 executor-2 的数据库扫描重做
一遍，其它组不用动。」

**动作（A15 分级路径）**：

1. **Structured event 匹配**：在 L0 recent_tail 里检索 "executor-2"
   是否能对上当前活跃 delegation tree 的 `agent_runs.run_id`。
   匹配成功 → `run-L3-exec-2`。
2. **Rule extraction**：关键词 "重做 / rerun" + "数据库扫描" →
   归类为 "partial retry with scope filter"。
3. 因为 scope "数据库扫描" 仍需解释（是 SQL 注入规则？还是 DB
   连接仓库？），**小模型兜底**生成 `suggested_next_action`
   payload `{target_run:'run-L3-exec-2', scope:'sast-rule-set:sqli-*',
   action:'retry_subtree'}`，置 `status='pending_user_confirm'`。
4. 因为用户原话含"重做一遍"已是高置信显式指令，L0 直接 apply
   该 suggestion 为 confirmed（`status='applied'`），写一条
   `session_state_item_events(mutation='apply_suggestion')`。
5. L1 响应 apply：派**新** `agent_runs` row `run-L3-exec-2-retry`，
   `retry_of='run-L3-exec-2'`、`parent_run_id='run-L1-orch'`、
   `delegation_id='dl-exec-2-retry'`。`retry_scope` 字段（见 §8
   建议新增）= `subtree` —— 意味着 L4 reviewer-2 也重派。
6. 原 `run-L3-exec-2` 不杀死，`status='superseded'`（新 enum），
   保留其已产出的 artifact 作历史（合规审计用）。

**manifest zone 对比**：
- L0：`recent_tail` +120（用户指令） / `session_anchor` zone 新
  增 1 条 retry marker，总 ≈ 4000（本次不加新 zone）。
- L1 下一轮：`delegation_state` 新增一条 `exec-2-retry` row +
  保留 `exec-2` 为 superseded；`plan_todo` 保持 4 组 executor
  的视图但 exec-2 换指针到新 run。
- L3 exec-2-retry 本级：**全新 manifest**，不继承 exec-2 的
  recent_tail。仅在 `retrieved_facts` zone 挂一条 "previous run
  diagnosis: rule set missed sqli-*" 指向原 run 的 L4 summary。

**LLM 看不到**：L0 的 LLM 调用**没发生**——A15 rule + 小模型
那一层就把意图转化成 structured `retry_of` 事件。主 LLM 从头到
尾没看到"脑补这是新任务"的机会。

### T9 · 所有 executor 完成，reviewer triage 收敛

**动作**：4 个 exec 全部 `status='completed'`（含 exec-2-retry），
各自 reviewer 完成 triage。`session_state_items(category=
'delegation_state')` 总计 11 条（1 plan + 4 exec + 4 review +
2 retry-pair）。各 reviewer 汇总 real-positive finding，数字分布
约：exec-1: 8 / exec-2-retry: 11 / exec-3: 6（含 T7 的 critical）
/ exec-4: 5，合计 30 条 severity ≥ high。每 executor 写
`category='summary', version=2` 最终 summary（each ≤ 400 token）。

**manifest zone**：各 L3 写 summary 行后 `status='completed'`；L1
在下一轮 orchestration 被唤醒时 manifest `reason='delegation_finalize'`。

### T10 · L1 聚合并生成精简报告（L0 最终交付）

**用户原话**：「收尾。按仓库分组出最终报告，每条漏洞附 rule_id /
复现指纹 / 首次发现时间，别放误报、别把 reviewer 内部讨论塞进来。」

**动作**：L1 派 **独立** reporter 子 run `run-L2-report`（新 L2，
与 planner 并列），`parent_run_id='run-L1-orch'`、`delegation_id=
'dl-report'`。reporter 做的是 **结构化投影 join**：读
`session_state_items(category='delegation_state')` 的 4 条 exec
最终 summary + `category='decision'` 里 T8 的 retry 决策记录 +
`session_artifacts` 按 severity >= high 过滤的 finding 条目。
**不回放**任何 L3/L4 transcript。

**manifest zone**（reporter 本级）：`retrieved_facts` 3800
（30 条 finding 全文 + 各 exec 的 one-line summary + 1 条
retry decision）/ `session_anchor` 500 / `plan_todo` 200 /
`recent_tail` 800 / `tool_previews` 0 / 总 ≈ 7700，`reason=
'tree_structured_report'`（与 S06 T35 同义）。

L0 界面：报告以 Markdown 呈现，顶部 banner 留着 T7 的
critical alert；点击每条漏洞可下钻到原始 artifact（由
`session_artifacts` 独立 URL 承载）。L1 run `status='completed'`，
L0 session 切 `status='archived'`。

**LLM 看不到**：T1 以来的全部 recent_tail；任何 L3/L4 原始
transcript；任何 reviewer 的内部推理。报告只由 `decision` +
`summary` + `artifact` 三张表的结构化投影合成。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| 5 层 parent_run_id 链按 repo 名反查 run_id（T6） | `agent_runs.parent_run_id` + `session_state_items` 按 item_key 建 index；建议加 `ancestor_path` 列避免递归 CTE（§8） |
| L0 manifest 绝不能看 L3/L4 原文 | A4 每层 manifest 独立；`context_manifests.run_id` 区分；L0 只从 `category='delegation_state'` one-line summary 取数 |
| 4 个 exec × 13 仓库 raw 输出合计 > 200 MB | A9 artifact_ref + preview_text；raw 永不进任何 zone，triage 后摘要 ≤ 400 token 写 L4 summary |
| T7 critical bubble 不等报告收尾 | `session_state_item_events(mutation='bubble_up')` 纯投影事件链；不触发新 LLM 调用 |
| T8 "重做 executor-2" 意图识别 | A15 分级：structured event（匹配已有 run_id）→ rule（"重做 / rerun"）→ 小模型 → `suggested_next_action` |
| T8 retry 子树语义 | `agent_runs.retry_of` + 新字段 `retry_scope='subtree'`；原 run 切 `superseded` 保 artifact（§8） |
| 跨层查询不重放 transcript | §History Reconstruction Modes 需补"Delegation tree drill-down"第 4 模式：只读投影查询 |
| 每层独立 manifest 的 observability | `context_manifests` 按 `run_id` 索引；`reason` 枚举补 5 个 delegation_* 值；T10 `tree_structured_report` 复用 |

## 5. 预期 state projection 演化

4 个关键节点快照。`agent_runs` 按状态计行；`session_state_items`
按 category（括号为 active）；delegation tree 深度 = 从 L0 起算的
最大祖先链长度。

**T2（planner 派发后）**：

| 项 | 值 |
| --- | --- |
| agent_runs 总数 | 2（L1 running + L2 running） |
| parent_run_id 链最大深度 | 2（L1 ← L2） |
| session_state_items / delegation_state | 1 (1)，item_key='plan' |
| session_state_items / summary | 0 |
| context_manifests 累计 | 2（L1×1 + L2×1） |

**T5（executor-2 派出 reviewer 后）**：

| 项 | 值 |
| --- | --- |
| agent_runs 总数 | 7（L1 + L2-completed + 4 × L3 + 1 × L4） |
| parent_run_id 链最大深度 | 4（L1 ← L3 ← L4） |
| delegation_state | 6 (6)：plan(closed) + exec-{1..4} + review-batch-1 |
| summary | 1 (1)（L2 planner 最终 summary） |
| context_manifests 累计 | 约 14（每层 run 平均 2 轮） |
| session_tool_outputs 累计 raw bytes | ~35 MB（executor-2 前 3 仓） |

**T8（retry 派发后）**：

| 项 | 值 |
| --- | --- |
| agent_runs 总数 | 约 11（+ exec-2-retry + 对应 L4 retry + 中间 reporter 尚未派） |
| retry_of 非空行 | 2（exec-2-retry, reviewer-2-retry） |
| superseded status | 2（原 exec-2, 原 reviewer-2） |
| delegation_state | 10 (8 active + 2 superseded) |
| session_state_item_events 累计 | ~60（含 T7 bubble_up + T8 apply_suggestion + retry 建边） |
| anchor alerts | 1 (T7 critical) |

**T10（最终报告产出后）**：

| 项 | 值 |
| --- | --- |
| agent_runs 总数 | 约 15（全 completed，含 reporter） |
| delegation_state | 11 (0 active)，全 closed |
| summary | 6 (1 active root-level)，其余 archived |
| decision | 3（T2 分组选型 + T5 dedup 策略 + T8 retry scope） |
| session_artifacts severity ≥ high | 30 |
| L0 context_manifests 总数 | 3（T1 kickoff + T6 drill-down injected + T10 final） |
| 物理 delete | 0 |

## 6. Token 节省估算

**朴素方案**：把 5 层 transcript 全塞给 L0 的 LLM，按"让用户的
LLM 看到所有细节保证不丢"做事。

- 4 × executor × 13 repo × 每 repo 平均 ~8k transcript token
  = 416k，单 L3 层已溢出任何主流模型。
- 4 × reviewer × triage 讨论平均 ~12k = 48k。
- L2 planner + L1 orch 本级 ~8k。
- T7 critical 发现后，朴素实现会把 bubble 链上的所有讨论打包
  重注入 L0 context → 再加 ~6k。
- T8 retry 朴素会"整个子树重放一次" 再加 ~110k。
- T10 报告回合朴素需要看所有 finding 原始 JSON = ~180k（~3800
  条 finding × 平均 50 token）。
- **朴素方案在 T4 之后的任何一轮都会超出 200k 上限**，即便
  Claude 200k / GPT-4o 128k / Gemini 1M 这种长窗口，T10 的
  finding 原文也会塞到 180k+，input cost 严重失控。

**本设计方案**（按 zone 加总；"L0-view" 表示用户视角看到的
合成上下文；每层 L1/L3/L4 各自的 manifest 只列代表轮）：

| zone | L0 T1 | L1 T4 | L3 exec-2 T5 | L4 review T5 | L0 T6 drill | L1 T10 final |
| --- | --- | --- | --- | --- | --- | --- |
| system_static | 1200 | 1200 | 1200 | 1200 | 0（不走 LLM） | 1200 |
| tool_schemas | 1100 | 900 | 2400 | 600 | 0 | 700 |
| skills | 600 | 600 | 500 | 400 | 0 | 400 |
| session_anchor | 450 | 450 | 450 | 500 | 0 | 500 |
| plan_todo | 0 | 500 | 800 | 0 | 0 | 200 |
| recent_tail | 300 | 700 | 1400 | 900 | 0 | 800 |
| summary | 0 | 350 | 600 | 0 | 0 | 2100 |
| retrieved_facts | 0 | 0 | 0 | 400 | 0 | 3800 |
| tool_previews | 0 | 0 | 300 | 100 | 0 | 0 |
| delegation_state | 0 | 800 | 180 | 0 | 0 | 400 |
| workspace | 200 | 200 | 200 | 200 | 0 | 200 |
| **合计** | **3850** | **5700** | **8030** | **4300** | **0** | **10300** |

**对比**：

| 节点 | 朴素 (tokens) | 本设计 (tokens) | 节省 |
| --- | --- | --- | --- |
| L0 T6 drill | ~120k（如朴素做回溯 + 扫描上下文拼贴） | 0（纯 DB 查询） | 100% |
| L0 T7 bubble | ~6k（bubble 链注入） | 0（纯事件投影） | 100% |
| L1 T10 final | ~180k | 10.3k | 94% |
| 跨层最深（L4 T5） | 5 层全 transcript 回放 ~460k | 4.3k（自身 local） | 99% |

**关键结论**：本场景的核心节省不是"某一轮省多少"，而是
**token 随 delegation 深度恒定 O(子数)**——L0 看到的始终是
`Σ child-level one-line summary`（4×150 = 600 量级），不会随
层数递增。5 层增加的是 `agent_runs` 行数和 `session_state_items`
行数（关系型 O(n) 存储），不是 prompt token 指数膨胀。这一条
正是设计文档 §Next-Action Extraction Cost 与 §Context Assembly
需要补强的"深层 delegation token 模型"。

## 7. 潜在失败模式

**F1 · `parent_run_id` 链深度查询性能退化**：T6 用户追问某仓库
进度时，若实现走朴素递归 CTE（`WITH RECURSIVE ... parent_run_id
= ...`），在 5 层 × 15 run 下尚可，但一旦某日 executor 粒度变细到
每仓一 run（变成 65+ run × 5 层），单次 query 可能扫描几百行。
**可观测信号**：`idx_runs_parent (parent_run_id)` index 被用但
`Rows_examined` 持续增长；P99 latency 爬升。**防守**：为
`agent_runs` 增 `ancestor_path TEXT` 或 `root_run_id` 列，写入
时由父节点 path 拼接；查询变单索引 point lookup（见 §8）。

**F2 · bubble-up 机制未在设计中定义**：T7 依赖
`session_state_item_events.mutation='bubble_up'`，但设计文档
§Session State Projection 只列了一般 mutation（insert/update/
replace），bubble_up 是场景自造。若实现各自为政，**可观测信号**：
不同实现把 critical alert 路由到不同 category（有的写 anchor、
有的写独立 category）；L0 UI 无法统一订阅。**防守**：把
bubble_up 作为一等 mutation 类型 + 规定其 payload 必须含
`bubble_target_scope` 与 `original_item_id`，并列入 §Session
State Projection 的 mutation 枚举。

**F3 · `retry_of` 在深层 delegation 下语义未定义**：T8 的 retry
设计文档里 `retry_of` 字段仅在 `agent_runs` schema 出现一次，
未说明重做范围。实现若把 retry 理解为"单节点"会漏重派 reviewer，
理解为"整树"会把 planner 都重跑（浪费）。**可观测信号**：retry
后 L4 reviewer 仍持旧规则集；或反之，L2 planner 被误触发重跑
导致 52 仓库重新分组。**防守**：新增 `retry_scope ENUM('node',
'subtree','siblings_of')` 字段（§8）；L1 作为 retry 协调者，
以 `scope='subtree'` 语义默认重做 L3 + 其 L4。

**F4 · L0 看不到子层错误（L4 失败被 L3 吞掉）**：exec-2 的 L4
reviewer 若跑挂（小模型 OOM），exec-2 实现若"静默降级"——自己
回退做 triage 不通知父层——L0 只看到 exec-2 "running" 表象。
**可观测信号**：`agent_runs` 某 L4 row 长时间 `status=
'running'` 或突然 `'failed'` 而父 L3 `status` 仍 `running` 且
`delegation_state` row 的 `status` 未切 `degraded`。**防守**：规定
子 run 异常终止必须通过 bubble_up（或同类事件）强制向上一层
`delegation_state` 打标 `status='degraded' | 'failed'`，父层
manifest 必须渲染异常子 run 的 one-line error。

**F5 · 跨层 artifact ACL 不清**：exec-3 产的 `auth-gateway` finding
artifact，L1 orch 要拿来做 T10 report；若实现把 artifact 绑在
产生层 run_id 上做 ACL，父层访问时会 403。**可观测信号**：
T10 reporter 拿不到某些 finding artifact，报告里对应仓库一片空白；
或反之，artifact ACL 完全放开，L0 同屏用户（多人协作时）越权
看到本不该看的 finding。**防守**：artifact ACL 沿 `root_run_id`
（= L0 session 的最顶层 run）继承，父层默认可见子层 artifact
但标 `origin_run_id` 保留审计；跨 root 共享需显式 grant。

**F6 · `delegation_state` 递归渲染无边界策略**：T4 L1 manifest 里
4 条 child delegation_state 每条 150 token 尚可控；若 T8 retry 引入
第二棵 retry 子树，且每棵再被用户要求展开一层 nested summary，
`delegation_state` zone 可能爆到 2k+。**可观测信号**：
`context_manifest_items` 里 `zone='delegation_state'` 的
`token_estimate` 在 L1 manifest 上持续增长；某轮 `dropped_count`
突增因为 plan_todo 被挤出。**防守**：规定 `delegation_state`
zone 上限（如 800 token）+ 递归渲染最多 1 层深度；更深层只
显示 "(+N nested)" 占位，下钻通过 T6 的跨层查询按需获取。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Open Questions "Multi-agent web UX: are child agents first-class
   sessions, run children, or both?" —— **S10 给出的可行答案**：
   **两者都不单独足够**。child agents 在 `agent_runs` 层是 run
   children（父子通过 `parent_run_id`），但在 session projection
   层是"父 session 持有的 delegation_state 行 + 子 run 的只读
   引用"；对 **5 层及以上**深度，父层 session **不应**为每个
   子 agent 复制 `agent_sessions` 行（S06 为 1 层下派专建 child
   session 的做法在 5 层会让 `agent_sessions` 爆炸）。建议文档
   明确："run children is the default; spawn a full child
   `agent_sessions` only when the subagent needs independent
   durable identity (user-facing, cross-session reuse)"。
2. §Proposed Schema §1 Durable Runs — **建议补强**：`retry_of`
   只声明单字段，未规定重做范围。新增字段 `retry_scope VARCHAR(32)
   NOT NULL DEFAULT 'node'`（枚举 `node` / `subtree` / `siblings`），
   并与 `status='superseded'` 共同构成"原 run 保留审计 + 新 run
   承接执行"的标准模式。
3. §Proposed Schema §2 Session State Projection — **建议补强**：
   `session_state_item_events.mutation` 枚举应扩到
   `{insert, update, replace, archive, bubble_up, apply_suggestion}`。
   `bubble_up` 的 payload 强制含 `bubble_target_scope` 与
   `source_run_id`；`apply_suggestion` 的 payload 关联
   `suggested_next_action.id`。
4. §History Reconstruction Modes — **建议补第 4 模式**：
   "Delegation tree drill-down"，数据源 `session_state_items
   (category='delegation_state') + agent_runs(parent_run_id)`，
   用例"5 层 delegation 下找某叶子节点状态"，load 模式"单次
   index lookup by item_key + 2 次 summary row fetch"，**不走 LLM**。

**三条改进建议**：

- **delegation_state 行升级结构化字段**：在 `session_state_items`
  上为 `category='delegation_state'` 的 row 增加计算列或独立
  投影：`depth INT`（从 root 起算）、`root_run_id VARCHAR(128)`、
  `ancestor_path TEXT`（如 `L0/L1:run-L1-orch/L3:run-L3-exec-2`）。
  这三字段是深层 delegation 所有查询的公共前缀，避免 T6/T8
  类查询每次递归 CTE。
- **bubble-up 一等事件机制**：把 critical finding / child run
  degraded 这两类"子层异常/重大事件"统一成
  `session_state_item_events(mutation='bubble_up')` + 规定
  父层的 delegation_state payload 必须 patch `{bubble_seq, severity,
  source_run_id, original_item_id}`。L0 UI 只订阅
  `scope='root_session'` 的 bubble_up 事件即可渲染全局告警，
  不需要订阅每个子 run。
- **retry_scope 配合 superseded 状态构成 retry 契约**：
  `agent_runs.retry_of` 存在时必须同时写 `retry_scope`；原 run
  必须切 `status='superseded'` 而非物理删除；UI 在 delegation
  tree 上同时渲染新旧两颗子树，旧的灰化但点击仍可看 artifact，
  满足安全审计"操作留痕"的硬性需求。
