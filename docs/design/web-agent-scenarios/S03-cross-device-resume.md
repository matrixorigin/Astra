# S03 — 跨设备恢复：laptop → iPad → 同事 Chromebook → laptop

> Status: Draft v0.1
> DAUT: **A1, A3, A4, A14**
> 关联设计: [../web-agent-session-state.md](../web-agent-session-state.md)

## 1. 场景概览

**用户画像**：某金融科技公司 CTO Alice，日常 5% 写代码、60% review、35%
协作。今天要把生产集群的 k8s 部署脚本（helm chart + 4 套 overlay：
dev/staging/canary/prod）从 helm 2 的 `requirements.yaml` 迁到 helm 3
umbrella chart，并顺带把 hard-coded 的 image tag 抽成 argocd appset
values。**不让 astra 自动无审改仓库**，每一步要 diff 审。

**业务背景**：当天要在 3 个物理设备之间切换：
（1）09:00 公司 MacBook + 本地 repo（用 edge bridge）；
（2）10:30 地铁 iPad，走 4G/3G 抖动网络；
（3）14:00 咖啡馆借同事的 Chromebook 5 分钟查看 agent 方案，**该设备从
未登录过 astra**，全靠 MatrixOne 重建；
（4）17:00 回办公室 MacBook 继续到交付。

**一句话目标**：在 4 次设备切换里，session 状态、plan/todo 进度、
context manifest 连续一致；iPad/Chromebook 段不能触发 edge-only 工具；
回到 MacBook 不丢本地未推的 edge 侧变更。

| 指标 | 预计值 |
| --- | --- |
| 总轮数 | 13 回合（MacBook 4 + iPad 3 + Chromebook 3 + MacBook 4） |
| 跨设备切换 | 3 次 |
| workspace_state 变更 | 3 次（hybrid → cloud → cloud → hybrid） |
| tool_schemas 收缩/恢复 | 2 次 |
| 冷启动次数 | 2 次（iPad、Chromebook） |
| 产物 artifact | ~18（overlay diff、values schema、kubectl diff） |

## 2. 被测 DAUT

本场景核心是「浏览器只发 session_id + watermark，MatrixOne 重建一切」，
下列 4 条假设同时被压到边界。

| 编号 | 压测方式 |
| --- | --- |
| **A1** | iPad（T5）与 Chromebook（T8）均为空 IndexedDB，两次冷启动请求**必须不携带** `known_state_revision`，服务端一次性下发全量 hot projection；MacBook 回归（T11）携带 stale watermark，服务端只回 deltas；任何一次让浏览器**把本地 transcript 回传覆盖 MatrixOne** 的路径都算失败。 |
| **A3** | 每次换设备第一次打开 session，hot 路径只许 3 条有界查询：`GET /sessions/{id}/state`（索引：`user_id + session_id`）、`GET /sessions/{id}/transcript?limit=50`（`session_transcript_items(session_id, item_seq DESC)` 分页）、`GET /chat/runs/{run_id}`（活跃 run 一条）。不得扫 `conversation_log` 全表，不得在 `payload_json` 上 LIKE。 |
| **A4** | 换设备后的第一条 agent turn，必须复用上一台设备最近 manifest 的 `stable_prefix_hash` 与 `prompt_cache_key`——system_static + tool_schemas（cloud 子集与 hybrid 超集同源）+ skills + session_anchor 组成的前缀不变，只在 `recent_tail` 附加新 user 消息即可命中 provider 侧 prompt cache。Chromebook 段如果触发 turn，prefix 应仍可复用。 |
| **A14** | `workspace_state` 在 T1 为 `hybrid`（edge bridge 在线），T5 / T8 切到 `cloud`（edge 不可达），T11 切回 `hybrid`。`tool_schemas` 随之收缩：iPad/Chromebook 段 `edge_fs_write`、`edge_shell` 不得出现在 manifest 里；回到 MacBook 后重新上线。任何一次 cloud 段把 `edge_fs_write` 喂给 LLM 都算破坏设计。 |

## 3. 会话时间线

每回合字段：**用户动作 / 服务端响应（含真实 URL + watermark 数字）
/ manifest zone 变化 / LLM 看不到**。以下 watermark 取值是假设值，仅示
意设计契约，不代表实际实现基准。

### — MacBook 段（T1–T4）—

### T1 · 09:02 开 session（hybrid + edge bridge 在线）

**用户动作**：打开 astra web app，新建 session，标题「k8s helm 2→3
migration」，附上 repo 本地路径 `~/work/infra/platform-k8s`。edge
bridge 握手通过 `POST /edge/bridges/attach` 注册本机 tool manifest。

**服务端响应**：`agent_sessions` 新行，`workspace_state` 写入
`session_state_items(category='workspace_state')`，payload 为
`{"authority":"hybrid","edge_bridge_id":"eb_macbook_alice_01",
"cloud_workspace_id":"ws_cloud_k8s_01","primary":"edge"}`。初始
`state_revision=1`，`transcript_high_watermark=0`，
`run_event_high_watermark=0`。

**manifest zone（T1 首回合）**：`system_static` 1200 / `tool_schemas`
2400（hybrid 超集：cloud_tools + edge_fs_read/write + edge_shell +
kubectl_local）/ `skills` 700（`k8s-migration` + `helm-umbrella`）/
`session_anchor` 300 / `plan_todo` 0 / `recent_tail` 0 /
`workspace` 260（edge + cloud 双 workspace 指纹）。总 ≈ 4860，
`stable_prefix_hash=sha256:px_hybrid_v1`，`reason='initial_turn'`。

**LLM 看不到**：整个 repo 文件树（8000+ 文件）。edge bridge 只暴露
schema，不预上传内容。

### T2 · 09:18 出 plan（edge-side repo scan）

**用户动作**：「先别改——扫一遍当前的 `requirements.yaml` 和四套
overlay，给我一份迁移 plan 和风险清单，本地仓库不要动。」

**服务端响应**：Agent 触发 `edge_fs_read` 走 bridge，读 12 个 YAML；
`session_state_items` 新增 `plan_state`（8 阶段）+ 9 条 `todo_state` +
3 条 `decision`（umbrella chart 命名、values 分层、image tag 抽离
方案）。`state_revision=7`，`transcript_high_watermark=22`，
`run_event_high_watermark=18`。manifest 写入 `context_manifests`，
`reason='post_edge_scan'`，`dropped_count=0`。

**manifest zone**：`tool_previews` 900（12 个 YAML 的
preview_text，每个平均 75 tokens）/ `plan_todo` 620 / `recent_tail`
900 / 其他同 T1。总 ≈ 7130。

**LLM 看不到**：12 个 YAML 的完整正文（合计 ~18k tokens），只进
preview + artifact_ref；`chart_lock.json`（被 edge 返回但未选入）。

### T3 · 09:45 首次 edit（经 edge bridge）

**用户动作**：「先动 `charts/platform/Chart.yaml`，把 apiVersion 改成
v2，把 requirements 挪到 dependencies。改完给我 diff 我来 apply。」

**服务端响应**：Agent 走 `edge_fs_write`，写一个 patch 而非直接 apply；
`session_artifacts` 新增 1 条 `chart-yaml-patch-v1`，
`artifact_ref='artifact:...'`，`storage='edge+cloud_mirror'`（patch 文
本双写方便跨设备查看）。`state_revision=11`，
`transcript_high_watermark=40`，`run_event_high_watermark=34`。

**manifest zone**：`tool_previews` 从 900 → 520（只保留当前 patch
preview）/ `plan_todo` 700（phase `apiVersion_migration=active`）/
`recent_tail` 1100。总 ≈ 7400。

**LLM 看不到**：patch 的 diff 上下文 ±20 行以外的源文件内容；
`Chart.yaml` 的 git log。

### T4 · 10:20 合盖离开（session → idle）

**用户动作**：合上 MacBook。浏览器触发 `beforeunload`，把最新
watermark flush 到 IndexedDB：`state_revision=12`，
`transcript_high_watermark=45`，`run_event_high_watermark=40`。

**服务端响应**：活跃 `agent_runs` 标记 `status='idle'`（非取消，保持
resumable）；`session_state_items` 不动；无新 manifest。edge bridge
维持 heartbeat，10 分钟后 `edge_bridge_status='offline'` 自动写入
`workspace_state.payload.edge_bridge_online=false`，
`state_revision=13`。

**manifest zone**：本回合不写 manifest（非 LLM 回合）。

**LLM 看不到**：本回合根本没调用 LLM。

### — iPad 段（T5–T7）—

### T5 · 10:35 地铁上打开 iPad Safari（冷启动）

**用户动作**：Safari 打开 `https://astra.example.com/workspace/s_k8s_01`。
首次使用，IndexedDB 空；4G 信号 2 格。

**服务端响应**：客户端**不携带** `known_state_revision`，直接：

```text
GET /sessions/s_k8s_01/state
GET /sessions/s_k8s_01/transcript?limit=50
GET /chat/runs/run_k8s_01
```

返回 `state_revision=13`，`transcript_high_watermark=45`，
`run_event_high_watermark=40`，`page_hash=sha256:tx_p0_xxx`。服务端检
测到 `workspace_state.payload.edge_bridge_online=false`，**在响应里
把 `workspace_state.authority` 动态投影为 `cloud`**（持久化行不改，
只在 UI projection 里切；真正切换等 T7 触发 turn 时落库）。

**manifest zone**：本回合无 turn，不写 manifest。UI 读
`GET /sessions/s_k8s_01/context/latest`，命中 T3 的 manifest_id。

**LLM 看不到**：本回合 LLM 未被调用，所有数据直接来自 MatrixOne hot
projection，浏览器没有任何本地权威状态。

### T6 · 11:15 快速问进度（只读查询）

**用户动作**：「目前 plan 进度？`staging` overlay 那条我加个备注：
不要动 secretRef。」

**服务端响应**：追加备注不触发 agent turn，仅 `POST /sessions/
s_k8s_01/notes` 走 state_items 新增 1 条 `user_note`（`category=
'user_note'`）。`state_revision=14`，`transcript_high_watermark` 不变
（note 走独立子表，不污染 transcript）。4G 抖动两次，客户端以
`state_revision=13` 作 if-none-match，第 2 次重试成功。

**manifest zone**：无 manifest（非 LLM 回合）。

**LLM 看不到**：LLM 未被调用；`user_note` 的内容要等 T7 下一次
turn 时才进 `session_anchor`。

### T7 · 12:04 追加一个 agent turn（cloud-only 工具）

**用户动作**：「既然不能碰本地 repo，你用云端模板给我生成一个
`staging` overlay 的 patch 草稿，保留 secretRef 不变，把 replicaCount
从 2 → 3，加一个 `NODE_ENV=staging`。我回办公室再合并。」

**服务端响应**：第一次真正跑 LLM。`workspace_state.authority` 正式
落库为 `cloud`，`session_state_item_events` 写入 1 条 mutation
（`from=hybrid,to=cloud,reason=edge_offline_user_turn`）。
`ToolSchemaSelector` 输出 cloud 子集：移除 `edge_fs_write`、
`edge_shell`、`kubectl_local`，保留 `yaml_render`、
`template_patch`、`cloud_fs_write`。Agent 生成 patch 写入
`session_artifacts` 的 cloud 存储（非 edge），
`artifact_ref='artifact:staging-overlay-patch-v1'`。
`state_revision=17`，`transcript_high_watermark=62`，
`run_event_high_watermark=58`。

**manifest zone**：`system_static` 1200 / `tool_schemas` 1500（cloud
子集，比 T3 少 900）/ `skills` 700 / `session_anchor` 380（含 T6
note）/ `plan_todo` 680 / `recent_tail` 900 / `tool_previews` 300 /
`workspace` 200。总 ≈ 5860。`stable_prefix_hash=sha256:px_cloud_v1`
（与 hybrid 版不同，因 tool_schemas 改了）；`prompt_cache_key` 新值。
`reason='cross_device_cloud_turn'`。

**LLM 看不到**：任何 edge-only 工具 schema；本地 repo 文件；T3 的
`edge_fs_write` 调用原始参数（仍活在 `agent_run_events` 里，不进
prompt）。

### — Chromebook 段（T8–T10）—

### T8 · 14:05 借同事 Chromebook（零本地缓存）

**用户动作**：咖啡馆，借同事 Chromebook 开 incognito 窗口，走 SSO 登
录 Alice 的 astra 账号，打开 session `s_k8s_01`。**该设备 IndexedDB
为空，浏览器从未接触过这个 session**。

**服务端响应**：与 T5 同形状的 3 条 hot 查询，但还多一条设备风险审
计：`auth_sessions` 记录 `device_fingerprint=fp_chromebook_XX`，
`trust_level='unknown_device'`，`session_state_items` 新增 1 条
`audit_state(category='audit_state', item_key='new_device_access')`。
`state_revision=18`，其余 watermark 与 T7 一致。UI 侧给用户一个黄色
横幅「新设备访问，操作被限制为只读」，限制由后端 enforce（非前端）。

**manifest zone**：本回合无 turn。`GET /sessions/s_k8s_01/context/
latest` 返回 T7 的 manifest_id，用于右侧 Context 面板显示。

**LLM 看不到**：LLM 未被调用；所有 UI 内容来自 MatrixOne，浏览器没
有任何本地持久状态（incognito）。

### T9 · 14:09 向上滚动历史（分页 + page_hash 验证）

**用户动作**：Alice 想看 T2 的 plan 推理，鼠标滚到顶。

**服务端响应**：

```text
GET /sessions/s_k8s_01/transcript?before_seq=1&limit=50
```

返回空（session 起点就是 T1），但若有更早 session 则命中
`session_transcript_items(session_id, item_seq)` 有界索引。客户端对当
前第一页做 revalidate：比较返回的 `page_hash` 与 T8 返回的
`page_hash`。一致则缓存（incognito，session 结束即丢）。

**manifest zone**：无 manifest。

**LLM 看不到**：LLM 未被调用；所有历史均来自 `session_transcript_
items` + `session_artifacts.preview_text`。

### T10 · 14:20 审阅并离开（不触发 turn，但未显式 logout）

**用户动作**：Alice 看完 T7 生成的 patch，满意，直接合上 Chromebook
交还同事，**没点 logout**。

**服务端响应**：后端由 `auth_sessions.idle_timeout`（建议 15 分钟
for `trust_level='unknown_device'`）在 14:35 自动标记会话失效；
`session_state_items` 追加 1 条 `audit_state(item_key='forced_logout
_unknown_device')`。`state_revision=19`。后续任何从该设备发来的请求
都会被拒（401）。这里依赖 session lease 机制（目前设计文档未显式定
义，见 §8 反馈）。

**manifest zone**：无 manifest。

**LLM 看不到**：Chromebook 段整个没触发任何 LLM turn，是**纯读路径**。

### — MacBook 回归段（T11–T13）—

### T11 · 17:03 回办公室打开 MacBook（stale cache + delta sync）

**用户动作**：MacBook 从休眠唤起，浏览器 IndexedDB 存的是 T4 的
watermark：`state_revision=12`，`transcript_high_watermark=45`，
`run_event_high_watermark=40`。edge bridge 自动重连握手。

**服务端响应**：

```text
GET /sessions/s_k8s_01/state?known_state_revision=12
GET /sessions/s_k8s_01/transcript?after_seq=45
GET /chat/runs/run_k8s_01/events?after_idx=40
```

服务端算差集：state 侧 delta 是 T6 的 note + T7 的 workspace 切换 +
T8/T10 的审计行 + T7 的 plan/todo 更新，总共 ~6 条 state_item mutation
（而非全量 19 行）。transcript delta 是 T7 的 17 条新 item。run events
delta 18 条。edge bridge 重新上线 → `workspace_state.authority` 从
`cloud` 切回 `hybrid`，`primary='edge'`，再写 1 条 state_item_event
`from=cloud,to=hybrid,reason=edge_reattached`。`state_revision=22`。

**manifest zone**：本回合无 turn，不写 manifest。

**LLM 看不到**：LLM 未被调用。关键点：**浏览器没有把本地 T4 的缓
存当权威推给服务端**，而是拉 delta 覆盖本地；A1 达标。

### T12 · 17:25 合并 iPad 段产物到本地 repo（edge turn + prefix cache 命中）

**用户动作**：「把 T7 那个 staging overlay patch 应用到本地 repo
`overlays/staging/`，不要 force，先 dry-run 给我看。」

**服务端响应**：Agent 触发 turn。`ToolSchemaSelector` 输出 hybrid 超
集（与 T3 相同），`stable_prefix_hash` 回到 `sha256:px_hybrid_v1`，与
T3 的一致——**provider 侧 prompt cache 命中**（system_static +
tool_schemas + skills + session_anchor 前缀），只需新送 `recent_tail`
增量（T6 note + T7 cloud patch artifact 的 preview + T12 user msg），
估算 ~900 tokens。Agent 调 `edge_fs_write`（dry-run 模式）生成本地
diff。新 manifest `reason='cross_device_merge'`，
`dropped_count=0`，`prompt_cache_key=pk_hybrid_v1`（与 T3 相同键）。
`state_revision=26`，`transcript_high_watermark=79`，
`run_event_high_watermark=75`。

**manifest zone**：`system_static` 1200 / `tool_schemas` 2400 /
`skills` 700 / `session_anchor` 420 / `plan_todo` 720 / `recent_tail`
900 / `tool_previews` 300 / `workspace` 260。总 ≈ 6900，其中前
4800 tokens 命中 prompt cache，仅 ~2100 tokens 为新内容。

**LLM 看不到**：Chromebook 段的审计细节（不进 prompt，只进审计面
板）；cloud artifact 原文（走 preview）。

### T13 · 18:10 跑 kubectl diff + 交付

**用户动作**：「对四套 overlay 跑一遍 `kubectl diff`，有冲突的列出
来，没冲突的给我 PR description。」

**服务端响应**：`edge_shell` 跑 `kubectl diff`，4 套 overlay 累计输出
~3.4 MB。`session_tool_outputs` 新增 1 行，`byte_size=3_564_032`，
`preview_text` 保留前 20 个 diff block（~1800 tokens），
`artifact_ref='artifact:kubectl-diff-delivery-v1'`。Agent 用
`category IN ('decision','plan_state','todo_state')` 一次性拉 14 条
结构化事实合成 PR description，存为 artifact
`delivery-report-v1`。`state_revision=31`，
`transcript_high_watermark=92`，`run_event_high_watermark=88`。

**manifest zone**：`tool_previews` 1800 / `retrieved_facts` 2200 /
`plan_todo` 300（基本全 done）/ 其他同 T12。总 ≈ 9980，
`reason='final_delivery_summary'`。

**LLM 看不到**：3.4 MB kubectl diff 原文；MacBook 段第一次的
manifest JSON 全文（只读结构化 state）。

## 4. 上下文压力点

| 压力来源 | 对应设计机制 |
| --- | --- |
| iPad/Chromebook 空缓存冷启动，必须在 <1.5s 呈现可操作界面 | A3 hot projection 3 条有界索引查询；`session_transcript_items` 分页；`context_manifests` 读 latest 单行 |
| 3 台设备不得让浏览器成为权威 | A1：请求仅携带 `session_id + known_state_revision + watermarks`，不上传 transcript；delta 由服务端算 |
| 地铁 4G 抖动下重试不能重放副作用 | `POST /chat/runs/{run_id}/input` 需带幂等键；`run_event_high_watermark` 做 SSE 断点续传 |
| iPad/Chromebook 不得触发 edge-only 工具 | A14：`workspace_state.authority` 驱动 `ToolSchemaSelector`，edge schema 不进 manifest |
| 切设备后仍想复用 provider prompt cache | A4：`stable_prefix_hash` 基于 (system_static + tool_schemas + skills + session_anchor) 内容哈希；同 authority 下跨设备一致 |
| Chromebook 零信任设备泄露风险 | `audit_state` + `auth_sessions.trust_level` + 短 idle_timeout；后端强制只读 |
| MacBook 回归后 stale cache merge | `state_revision` 单调递增，delta 由服务端下发，客户端 drop 本地分歧版本 |
| edge bridge 掉线但 session_state 里还写 hybrid | `workspace_state.payload.edge_bridge_online` 分字段追踪；projection 动态降级为 cloud，不改持久化 authority 字段直到真正 turn 触发 |

## 5. 预期 state 演化

4 个关键节点快照。括号内为 `status='active'` 的条数。

**T1（MacBook 开 session）**

| category | rows | 关键字段 |
| --- | --- | --- |
| anchor | 1 (1) | 迁移目标 + 不自动无审约束 |
| workspace_state | 1 (1) | `authority=hybrid, primary=edge, edge_bridge_online=true` |
| plan_state | 0 | — |
| todo_state | 0 | — |
| skill_hint | 2 (2) | `k8s-migration`, `helm-umbrella` |
| audit_state | 0 | — |

watermark: `state_revision=1`, `transcript_high_watermark=0`,
`run_event_high_watermark=0`. `tool_schemas` 可用集合：cloud + edge
超集（~14 tools）。

**T5（iPad 冷启动进入）**

| category | rows | 关键字段 |
| --- | --- | --- |
| anchor | 1 (1) | 不变 |
| workspace_state | 1 (1) | 持久化仍是 `hybrid`，但 `edge_bridge_online=false`，UI projection 呈现为 `cloud` |
| plan_state | 1 (1) | 8 阶段，version=1 |
| todo_state | 9 (9) | T2 生成 |
| decision | 3 (3) | umbrella / values 分层 / image tag |
| tool_ref | 12 (12) | T2 扫的 YAML preview |

watermark: `state_revision=13`, `transcript_high_watermark=45`,
`run_event_high_watermark=40`. **iPad 会话 UI 看到的 `tool_schemas`
可用集合**：cloud 子集（~8 tools），edge 类隐藏。

**T8（Chromebook 冷启动）**

相对 T5 只多了 T6 note 与 T7 结果：

| category | rows | 关键字段 |
| --- | --- | --- |
| workspace_state | 1 (1) | T7 已落库 `authority=cloud`，reason=`edge_offline_user_turn` |
| user_note | 1 (1) | T6 加的 staging 备注 |
| audit_state | 1 (1) | `new_device_access: fp_chromebook_XX, trust=unknown_device` |
| plan_state | 1 (1) | version=2（T7 推进） |
| todo_state | 12 (6) | 6 done / 3 active / 3 paused |

watermark: `state_revision=18`, `transcript_high_watermark=62`,
`run_event_high_watermark=58`. `tool_schemas` 可用集合：cloud 子集 +
**UI 只读 flag**；后端 enforce 拒绝任何 write 动作。

**T11（MacBook 回归 · delta sync 后）**

| category | rows | 关键字段 |
| --- | --- | --- |
| workspace_state | 1 (1) | `authority=hybrid` 再次（T11 切回） |
| audit_state | 2 (0) | Chromebook 的 `new_device_access` + `forced_logout_unknown_device` 均归档 |
| user_note | 1 (1) | 继续带 |
| plan_state | 1 (1) | version=2 |
| todo_state | 12 (6) | — |

watermark: `state_revision=22`, `transcript_high_watermark=62`,
`run_event_high_watermark=58`. `tool_schemas` 回到 hybrid 超集，
`stable_prefix_hash` 回到 T3 值。

## 6. Token 节省估算

**朴素方案**：每次打开 session 都把完整 transcript 喂 LLM + 完整 tool
schemas（不区分 hybrid/cloud）。

- T1 朴素 ≈ 本设计基线 4860（首回合无差异）。
- T2 ≈ 7130（也一致；尚未积累历史）。
- T5 iPad 冷启动若触发 turn：朴素把 T1–T4 全量 transcript 塞进去 ≈
  15k tokens + 全部 edge tool schema（edge_fs_write 等无用）~900 extra。
  本设计只推 hot projection + recent_tail（不需要立刻 LLM 调用）。
- T7 iPad 真 turn：朴素 ~15k transcript + 2400 tool_schemas = ~17.4k；
  本设计 5860，节省 ~66%。
- T8 Chromebook 打开：朴素会再次全量下发 transcript 到前端（3 MB+
  JSON）；本设计 hot projection + 1 页 transcript ≈ 140 KB 网络 payload。
- T11 MacBook 回归：朴素重新下发全部 19 条 state_items + 62 条
  transcript item + 58 个 run event ≈ 210 KB；本设计只下发 delta
  （~6 条 state + 17 条 transcript + 18 event）≈ 22 KB，节省 ~89%。
- T12 merge turn：若 `stable_prefix_hash` 不复用，provider prompt
  cache miss，整个 4800 tokens 前缀重新计费；本设计命中 cache，实际
  新计费 ~2100 tokens。按 Sonnet input $3/M，节省约 $0.008/次，但真
  实价值是 latency：cache hit 让 TTFB 从 ~1.8s 降到 ~0.4s。

**合计对比**：

| 指标 | 朴素 | 本设计 | 节省 |
| --- | --- | --- | --- |
| T7 单轮 prompt tokens | ~17.4k | 5.9k | ~66% |
| T11 delta sync payload | ~210 KB | ~22 KB | ~89% |
| T12 首字节延迟 | ~1.8s | ~0.4s | ~78% |
| Chromebook 段网络吞吐 | 全量 transcript 下发 ~3 MB | ~140 KB | ~95% |
| 跨设备整体 | edge tool schemas 无条件下发，Chromebook 产生 cloud 段 LLM 看到 edge 工具错觉 | 按 authority 动态裁剪 | 功能性节省（避免错用工具） |

**关键结论**：跨设备的节省不只是 LLM token，更大的部分是**网络
payload + 首屏时间 + provider cache hit**。authority 驱动的
tool_schemas 裁剪让 iPad/Chromebook 段 prompt 缩 35% 且避免 LLM 幻想
调用不存在的工具。

## 7. 潜在失败模式

**F1 · watermark 回退误判**：iPad T5 冷启动时 `state_revision=13`，
MacBook 10:20 合盖时本地 watermark=12。若 MacBook 17:03 回归时，客户
端判定逻辑错把「本地更大」当作权威（例如误用 `Math.max(local,
remote)`），可能把 iPad 期间 T6/T7 的更新覆盖掉。**可观测信号**：
`session_state_item_events` 里 T11 出现从 hybrid 回到 hybrid 的**空
mutation** 但 `current_revision` 倒退；前端 Console 报 "stale delta
ignored" 并丢 T7 patch。**缓解**：服务端是唯一 revision 权威，客户端
永远以服务端 `state_revision` 为准，本地 watermark 仅用于请求 diff
起点。

**F2 · Chromebook 未显式 logout 导致 session 泄露**：T10 Alice 合盖
交还设备，Chromebook 上的 cookie / token 没清。若同事 14:45 重新打开
浏览器仍能看到 session。**可观测信号**：`auth_sessions` 在 14:35 之
后仍有活跃行；`audit_state` 无 `forced_logout_unknown_device`。**缓
解**：`trust_level='unknown_device'` 必须配短 idle_timeout + server
端 revoke，不能仅靠前端 storage 清理；此机制当前设计文档未明确提（见
§8 反馈 2）。

**F3 · 同 run_id 被两设备同时看 SSE**：T7 iPad 触发 run，T8
Chromebook 打开时该 run 已 idle，但若 run 变成 active（例如 T12
MacBook 新 run），Chromebook 段的残余 session 还订阅旧 `run_id` 的
stream。**可观测信号**：服务端 SSE 下发相同 `run_event_idx` 到两个
连接，前端出现重复消息块；或 `POST /chat/runs/{run_id}/input` 从两
设备同时发来导致 run 状态机混乱。**缓解**：`run_event` 幂等键
（`run_id + event_idx`）+ 服务端对 `POST input` 校验设备
fingerprint；未信任设备的 input 直接 403。

**F4 · edge tool 可见但不可达的僵尸状态**：T5 iPad 进入时 edge 其实
已 offline，但若 `ToolSchemaSelector` 只看 `workspace_state.authority`
不看 `edge_bridge_online`，会仍把 `edge_fs_write` 塞进 prompt，LLM 会
调用然后卡在 bridge timeout。**可观测信号**：T7 manifest 里
`tool_schemas` zone 仍含 `edge_*` 工具；`agent_run_events` 出现
`tool_call_timeout` 密集；`total_estimated_tokens` 比预期多 ~900。
**缓解**：`ToolSchemaSelector` 必须合并 `authority` + 实时
`bridge_online` + `workspace_state_item_events` 最近状态，三者任一为
false 即剔除 edge schema。

**F5 · state_revision 冲突时 merge 策略缺失**：如果 iPad 和
Chromebook 几乎同时写入（例如两设备同秒 note / input），
`state_revision` 由后到者覆盖、先到者丢失（last-writer-wins）。
**可观测信号**：`session_state_item_events` 里同一 `item_key` 出现
两条 timestamp 极接近的 insert，但其中一条的 payload 在后续 projection
中消失；用户报告「我那条备注不见了」。**缓解**：针对 append-only
category（note / audit / decision）用 append-never-overwrite；针对
覆盖类（plan_state / workspace_state）服务端 CAS（`expected_revision`
失败则 409 返给客户端要求重新 rebase），目前设计文档对这块
gap 未定义（见 §8 反馈 1）。

## 8. 对设计的反馈

**回到设计文档的具体章节**：

1. §Web Client Cache（L373–L432） — **建议补强 state_revision gap 语
   义**：当前文档定义了 `state_revision` 与 `known_state_revision`
   的 happy path delta 语义，但没定义 gap/冲突时的行为：
   （a）客户端 `known_state_revision=12` 而服务端已到 22，delta 返
   回形态（一次全量 vs N 条增量）；（b）两客户端并发写时的 CAS 契
   约与 409 状态码；（c）客户端本地 revision 落后超过阈值（如 100）
   时是否直接全量重置。S03 的 T11 / T5 / T8 三次冷/暖启动都依赖这
   套语义，实现时没明确约束会直接在 F1/F5 爆雷。建议加一小节
   「Revision Reconciliation」固化这三类路径。

2. §Web Client Cache + §API Additions — **建议引入 device
   fingerprint + session lease**：设计里把"信任"隐含为
   `auth_sessions` 存在即可访问，但 S03 Chromebook 段证明需要更细
   粒度：`device_fingerprint`、`trust_level ∈ {trusted, new_device,
   unknown_device}`、`session_lease(device_id, expires_at)`，以及
   `POST /sessions/{id}/device/revoke`。当前文档完全没提这层，实现
   很可能做成前端 localStorage 自清，F2 会直接暴露。

3. §Cloud Workspace vs Edge Workspace（L1412–L1424） — **建议补强
   edge 降级 API 与 projection 一致性**：现在只说
   `session_state_items(category='workspace_state')` 持有 authority，
   但没有：（a）`edge_bridge_online=false` 时 projection 是否自动
   降级、何时从 UI-only 降级升级为持久化 authority 切换；（b）
   `ToolSchemaSelector` 如何消费 bridge 实时状态；（c）
   `POST /edge/bridges/{id}/detach` 的语义与重新 attach 的
   identity 对账（同 user 不同 MacBook 如何判同一 edge）。S03 的
   T4→T7→T11 三次切换全部压在这条路径上，F4 僵尸工具是最大的实
   现陷阱。建议这一节增加「Degradation Semantics」子节，明确四个
   状态（online/offline/reconnecting/detached）与 authority 投影
   规则。
