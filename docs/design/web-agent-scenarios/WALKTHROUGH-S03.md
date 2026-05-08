# S03 — Walkthrough: 跨设备恢复 (MacBook → iPad → Chromebook → MacBook)

> Status: Walkthrough v1 (基于 v0.2 设计 + G1–G19 patch)
> Scenario: [S03-cross-device-resume.md](./S03-cross-device-resume.md)
> Design: [../web-agent-session-state.md](../web-agent-session-state.md)
> Gaps invoked: **G13** (device lease + revision), **G11** (reachability),
> **G19** (watermark atomicity), **G16** (skill activation), **G10** (budget)

---

## §1 场景标识与 v0.2 映射

**一句话场景**: CTO Alice 在 13 回合内跨 4 次设备切换
（MacBook→iPad→Chromebook→MacBook）完成 helm2→3 迁移，期间经历 2 次冷启动、
3 次 workspace authority 切换、1 次零信任设备只读审阅、1 次 stale-cache
delta 同步。

**被测 DAUT**: A1（浏览器非权威）、A3（hot path 3 条有界查询）、
A4（prompt cache 跨设备复用）、A14（authority 驱动 tool schemas 裁剪）。

**v0.2 关键章节 / 行号**:
- §Revision Reconciliation and Device Lease @ **L438–498**（G13：
  `session_device_leases` 表 + `trust_level` 三态 + `monotonic_id` vs
  `revision_hash` 分离 + `POST /sessions/{id}/device/revoke`）
- §Workspace Reachability and Degradation Semantics @ **L2665–2716**
  （G11：`reachability_probe` + online/reconnecting/offline/detached
  四态 + 200ms HEAD cloud relay 探活 + detach/reattach API）
- §Web Event Watermark Atomicity @ **L500–550**（G19：IndexedDB 事务
  apply protocol、`(run_id, event_idx, event_hash)` 幂等、多 tab
  `BroadcastChannel`、gap recovery 协议）
- §Personal Skill Activation and Evaluation @ L2128–2180（G16：
  `active_skill` scope=session + 冻结 `version_id`）
- §Small-Window Budget Template @ L2410–2440（G10：`budget_v1_8k`）
- §Budget Policy v1 @ L2381–2406（zone cap 基线）

---

## §2 运行前提

**Schema 初值**（T1 前）:
- `agent_sessions` 空；T1 写入 `s_k8s_01 / u_alice / active`
- `agent_runs` 空；T2 触发首个 run `run_k8s_01`
- `session_state_items` T1 只有 anchor + workspace_state + 2×active_skill 共 4 行
- `context_manifests` 空；`session_device_leases` 已有 1 条
  `(ls_mb_01, dev_macbook_alice_01, fp_mb_XX, trust_level='trusted',
  status='active', expires_at=+30d)`（历史 lease）
- `workspace_state` T1 `payload_json`:
  ```json
  {"authority":"hybrid","edge_bridge_id":"eb_macbook_alice_01",
   "cloud_workspace_id":"ws_cloud_k8s_01","edge_status":"online",
   "reachability_probe":{"last_ok_at":"09:02:00Z","rtt_ms":38}}
  ```
- IndexedDB（MacBook）`session_watermarks` 空

**Workspace**: hybrid 初始 14 条 tool：cloud 8 条（`yaml_render`、
`template_patch`、`cloud_fs_write`、`webfetch`、`sql_dry_run`、
`kubectl_remote_ro`、`markdown_render`、`cite_lookup`）+ edge 6 条
（`edge_fs_read`、`edge_fs_write`、`edge_shell`、`kubectl_local`、
`git_status`、`git_diff`）。

**Context window**: MacBook/iPad Sonnet 4 / 200k →
`budget_template_id='budget_v1_200k'`。Chromebook 纯只读不载入 budget。
`tokenizer_id='claude-sonnet-4-20250514'` 跨设备一致（否则 G10
L2436–2438 强制 `stable_prefix_hash` 重算，A4 不成立）。

**Skill**: T1 Alice 勾选 `k8s-migration@v3.2` + `helm-umbrella@v1.0`
写 2 条 `active_skill` scope='session' 冻结 `version_id`（G16 L2130）。

---

## §3 端到端运行轨迹（13 回合）

### T1 · 09:02 开 session（MacBook, hybrid + edge online）

**User**: 打开 astra web，新建 session 标题 "k8s helm 2→3 migration"；
勾选 2 个 skill；edge bridge desktop app 握手绑 `~/work/infra/platform-k8s`。

**Server**: `INSERT agent_sessions`；写 4 条 `session_state_items`；
`session_device_leases` upsert `(fp_mb_XX, trust_level='trusted',
last_monotonic_id=0, expires_at=+30d)`（G13 L454–473）；
`reachability_probe` 首次写入 `last_ok_at=09:02:00Z, rtt_ms=38`
（G11 L2678–2684）；`monotonic_id=1`，
`revision_hash=sha256(s_k8s_01|1|fp_mb_XX|0|0|H(state))`（G13 L446）。

**User sees**: SSE `session_created` + `workspace_state_projection
{authority:hybrid, edge_status:online}`；Context 面板首次 manifest
`system_static 1200 / tool_schemas 2400 / skills 700 / session_anchor
300 / workspace 260 = 4860`，`stable_prefix_hash=px_hybrid_v1`，
`reason='initial_turn'`；edge bridge 状态绿灯。IndexedDB 事务
（G19 L519–526）写入 `session_watermarks(s_k8s_01, 0, 0, 1)`。

### T2 · 09:18 出 plan（edge_fs_read 扫 repo）

**User**: "扫 requirements.yaml + 四套 overlay，给我 plan，仓库别动。"

**Server**: 新 run `run_k8s_01`；`edge_fs_read` 读 12 个 YAML 写 12 条
`session_tool_outputs`（`preview_text ≤ 80`、`normalize_version='v1'`
符合 G17）；写 `plan_state`（8 阶段 version=1）+ 9 条 `todo_state`
+ 3 条 `decision`；`run_event_idx=18`；`monotonic_id=7`、
`transcript_hwm=22`、`run_event_hwm=18`；manifest `reason='post_edge_scan'`。

**User sees**: SSE `run_started → thinking → 12×(tool_call,tool_result)
→ plan_state_write → run_completed`；Plan 面板 8 阶段树；Context
manifest ~7130（`tool_previews 900 / plan_todo 620 / recent_tail 900`）。

### T3 · 09:45 首次 edit（edge_fs_write 生成 patch，不 apply）

**User**: "改 `Chart.yaml`，apiVersion→v2，requirements→dependencies，
给 diff 我审。"

**Server**: `edge_fs_write` 生成 patch（not apply），写
`session_artifacts(chart-yaml-patch-v1, storage='edge+cloud_mirror')`
双写让 iPad/Chromebook 也能看 preview；`plan_state.version` 不 bump
（只改 phase 状态，遵守 G2 compaction 白名单不变量）；`monotonic_id=11`、
`transcript_hwm=40`、`run_event_hwm=34`；manifest
`reason='post_edge_first_edit'`。

**User sees**: 右侧 diff viewer 7 行 hunk；Context manifest
`tool_previews 520 / plan_todo 700 / recent_tail 1100` 总 ~7400；
`stable_prefix_hash=px_hybrid_v1`（前缀未变，G10 L2436 保证 provider
prompt cache hit）。

### T4 · 10:20 合盖离开（session→idle, edge heartbeat 降级）

**User**: Alice 点 Reject（不 apply），合 MacBook 盖子。
`beforeunload` 触发 IndexedDB 事务（G19 L519–526）flush 到
`run_event_hwm=40`。

**Server**: 活跃 run `status='idle'`（可恢复）；
`session_device_leases.updated_at=10:20, last_monotonic_id=12`；9 分钟
后（10:29）cloud relay 连续 3 次 HEAD 探活失败，按 G11 L2697–2700 写
`edge_status: online→reconnecting`；2 分钟后 `reconnecting→offline`；
`authority` **持久化字段暂不改**（G11 仅规 projection 层，见 §4 F2）；
`monotonic_id=13`。

**User sees**: 本回合无 LLM 调用无新 manifest；SSE 连接在 10:21 主动 close。

### T5 · 10:35 iPad Safari 冷启动（全新设备 + 4G 抖动）

**User**: 地铁打开 `https://astra.example.com/workspace/s_k8s_01`。
iPad 从未登录 astra，IndexedDB 空。

**Server**: 前端**不携带** `known_state_revision`（A1 契约），hot path
发 3 条有界查询（A3）：`GET /state` + `GET /transcript?limit=50` +
`GET /chat/runs/run_k8s_01`。auth 通过后 upsert
`session_device_leases(fp_ipad_YY, trust_level='new_device',
expires_at=+7d)`（G13 L488 `new_device` 短于 trusted 的 30d）；检测
`edge_status=offline`，**UI projection** 把 `authority` 映射为 `cloud`
+ `tool_whitelist` 剔除 6 条 edge 保留 8 条 cloud；返回 `monotonic_id=13`、
`revision_hash=sha256(...|fp_ipad_YY|...)`（device-specific，hash 必异
于 MacBook 本地缓存）；写 `audit_state(item_key='new_device_access')`；
`monotonic_id=14`。

**User sees**: 2 个 RTT ~800ms 后首屏；banner "新设备登录，写操作需二次
确认"（G13 L488 step-up）；Context 面板 `GET /context/latest` 命中 T3
manifest_id 但展示层按 projection 裁剪 tool 列表（§4 F1）。Plan 8 阶段
+ Artifact `chart-yaml-patch-v1` preview（走 cloud_mirror）。

### T6 · 11:15 快速加备注（非 LLM）

**User**: "staging overlay 那条我备注：不要动 secretRef。"

**Server**: `POST /sessions/.../notes` → `INSERT session_state_items
(category='user_note', item_key='note_staging_secretref')`；
`transcript_hwm` 不变（独立子表）。4G 抖动第一次请求超时；客户端带幂等键
`client_request_id=req_note_XX` 重试，server 去重；`monotonic_id=15`。

**User sees**: note 渲染到 plan_todo 侧栏 phase 下；无 LLM；note 内容
**尚未进 session_anchor**（等 T7 重建 manifest 才挂上）。

### T7 · 12:04 iPad 首次真 agent turn（cloud-only）

**User**: "用云端模板给我生成 `staging` overlay patch 草稿，secretRef
保留，replicaCount 2→3，加 NODE_ENV=staging。回办公室再合。"

**Server**: iPad 段首次 LLM 调用。`ToolSchemaSelector` 合并三信号
（G11 L2697 + §4 F4）：`authority=hybrid` 持久化仍 hybrid；但
`edge_status=offline` + `probe.last_fail_at=10:31` 任一 fail 即剔除
edge schema。**持久化层**此时 `authority` 从 `hybrid` 落库 `cloud`，
写 event `{from:hybrid, to:cloud, reason:'edge_offline_user_turn'}`。
`new_device` trust 的 step-up 因 Alice passkey 已绑定被动 ok。起
`run_k8s_02`，cloud 工具集 8 条进 tool_schemas；调 `yaml_render` +
`template_patch` 写 `session_artifacts(storage='cloud',
staging-overlay-patch-v1)`；`plan_state.version=2`；`monotonic_id=17`、
`transcript_hwm=62`、`run_event_hwm=58`。

**User sees**: SSE 4G 抖动下重连 2 次，G19 L531–533 gap recovery
`last_index=last_ok_idx` 补拉；manifest `system_static 1200 /
tool_schemas 1500 / skills 700 / session_anchor 380 / plan_todo 680
/ recent_tail 900 / tool_previews 300 / workspace 200 = 5860`；
`stable_prefix_hash=px_cloud_v1`（**变了** — tool_schemas 内容哈希变，
新 `prompt_cache_key=pk_cloud_v1`）。跨 authority 必然 cache miss。
Patch 预览渲染右侧 diff。

### T8 · 14:05 Chromebook（同事设备，零信任）

**User**: 咖啡馆借同事 Chromebook 开 incognito，SSO 登录 Alice，打开
`s_k8s_01`。IndexedDB 空 + 陌生 fingerprint。

**Server**: hot path 同 T5 3 条。upsert `session_device_leases
(fp_cbook_ZZ, trust_level='unknown_device', expires_at=NOW+15min)`
（G13 L490 + 短 idle_timeout）；写 `audit_state(unknown_device_access)`；
服务端 enforce 返 `tool_whitelist=[]` + `readonly=true`（后端拒写：所有
`POST /chat/runs/.../input` 403）；`workspace_state` 持久化仍 `cloud`，
`edge_status=offline`；`monotonic_id=18`。

**User sees**: 黄色横幅 "新设备（未识别），只读模式，15 分钟后自动登出"；
Plan/Todo/Artifact 面板可读；Send/Approve 按钮灰；Context 面板显示 T7
manifest_id 只读 snapshot；incognito 不写 IndexedDB 仅内存缓存。

### T9 · 14:09 向上滚动历史（分页 + page_hash 校验）

**User**: Alice 滚到 transcript 顶部回看 T2 plan 推理。

**Server**: `GET /transcript?before_seq=1&limit=50` 空（T1 是起点）；
客户端对 page 0 做 revalidate（`after_seq=0&limit=50`），比对
`page_hash` 与 T8 首屏返回值，一致则内存缓存；走
`session_transcript_items(session_id, item_seq)` 有界索引无全表扫；
`monotonic_id` 不变。

**User sees**: 滚动流畅无 flicker；artifact preview 来自
`session_artifacts.preview_text` 不拉全量 body。Alice 若快速滚 3 次，
incognito 每次 revalidate 走网络（§4 F6）。

### T10 · 14:20 合盖离开（未显式 logout）

**User**: Alice 确认 T7 patch 满意，合 Chromebook 交还同事，
**没点 logout，没清 cookie**。

**Server**: Chromebook cookie / access_token 仍有效。
`session_device_leases.expires_at=14:35`；14:35 cron 扫 → `UPDATE
status='expired', revoked_at=14:35`；写
`audit_state(forced_logout_unknown_device)`；`monotonic_id=19`。
**关键问题：G13 `POST /sessions/{id}/device/revoke` 是否自动调？**
文档 L495 该 API 为用户/管理员**显式** revoke；自动过期走 lease
`expires_at` 被动过期——二者路径并存，但**未说明是否 emit 同样 SSE
`device_revoked` 事件**（§5 gap 候选 B）。同事 14:45 重开 tab，本地
cookie 仍带但 server 看到 lease `expired` → 401 跳登录页。

**User sees**: Alice 无感知；无 LLM；同事 14:45 重开 → 登录墙。若 SSE
push 未做，Chromebook localStorage 内容残留到手动清 cache（§4 F3
安全风险）。

### T11 · 17:03 MacBook 唤醒（stale cache + delta sync + edge reattach）

**User**: 回办公室开 MacBook，浏览器从 swap 恢复，IndexedDB 存 T4
watermark（`monotonic_id=12, transcript_hwm=45, run_event_hwm=40`）；
edge bridge 重握手。

**Server**: 3 条 hot 查询带 `known_state_revision=12`：
- `GET /state?known_state_revision=12`：server 比 `monotonic_id`
  （G13 L449 "compares monotonic_id, not full hash"）12 < 19，走
  **delta** 路径（L478–480）；返回 6 条 mutation：T5 audit / T6 note /
  T7 workspace→cloud / T7 plan v2 / T8 audit / T10 audit
- `GET /transcript?after_seq=45`：17 条 delta（T7 run_k8s_02 items）
- `GET /chat/runs/run_k8s_02/events?after_idx=0`：本设备未订阅过，返全量

同时 edge bridge 走 `POST /edge/bridges/eb_macbook_alice_01/reattach`
（G11 L2714）按 L2702–2709 对账 `edge_bridge_id + device_fingerprint +
user_auth` 匹配 → `edge_status: offline→online`；**authority 是否自动
回 hybrid？** G11 L2708 只说 status→online，authority 推导隐晦
（§5 gap 候选 A）；S03 §3 倾向立即切回 hybrid 写 event
`{from:cloud, to:hybrid, reason:'edge_reattached'}`。客户端
`revision_hash` vs 服务端返回必然不等（iPad 写入 hash 基于
`fp_ipad_YY`，本地基于 `fp_mb_XX`）；但 monotonic_id 单调无 rollback
→ 接受 delta（§4 F4：v0.2 未明确此情形）；`session_watermarks` 同一
IndexedDB 事务推进到 `monotonic_id=22`（G19 L519–526 原子性）。

**User sees**: 1.1s 内 UI 从 T4 快照平滑过渡到 T11；Context 面板右上
banner "已同步 iPad 段 3 条更新"；Plan 面板 v2；Artifact 多
`staging-overlay-patch-v1`；edge bridge indicator 从灰变绿；无 LLM。

### T12 · 17:25 合并 iPad patch 回本地（edge turn + prefix cache 命中）

**User**: "把 T7 的 staging overlay patch 应用到本地 repo
`overlays/staging/`，dry-run 先，别 force。"

**Server**: 起 `run_k8s_03`；`ToolSchemaSelector` 三信号全绿
（authority=hybrid + edge_status=online + probe.last_ok_at 新鲜）→
hybrid 超集回归；tool_schemas zone 与 T3 位等内容哈希一致 →
`stable_prefix_hash=px_hybrid_v1`（**A4 命中**）。隐藏假设：`active_skill`
依赖 `version_id` 稳定（§4 F5），S03 内未升级 → hash 一致。Agent 调
`edge_fs_read` + `edge_fs_write` 生成 dry-run artifact
`merged-staging-diff-v1`；`monotonic_id=26`、`transcript_hwm=79`、
`run_event_hwm=75`；manifest `reason='cross_device_merge'`。

**User sees**: manifest `system_static 1200 / tool_schemas 2400 /
skills 700 / session_anchor 420 / plan_todo 720 / recent_tail 900 /
tool_previews 300 / workspace 260 = 6900`；前 4820 tokens 命中 provider
prompt cache（Anthropic cached_tokens ~10% 计费）；TTFB ~380ms（vs
miss ~1.7s）。dry-run diff 渲染右侧。

### T13 · 18:10 跑 kubectl diff 收尾 + PR description

**User**: "对四套 overlay 跑 `kubectl diff`，有冲突列出，没冲突给我
PR description。"

**Server**: `edge_shell` 跑 `kubectl diff` ×4，总输出 3.4 MB；写
`session_tool_outputs(byte_size=3_564_032, preview_text=前 20 block
~1800 tokens, artifact_ref='kubectl-diff-delivery-v1')`；按 `category
IN ('decision','plan_state','todo_state')` 结构化查询（A3 走索引
`(session_id, category, status)`）拉 14 条事实合成 PR description 存
`delivery-report-v1`；`monotonic_id=31`、`transcript_hwm=92`、
`run_event_hwm=88`。

**User sees**: manifest `tool_previews 1800 / retrieved_facts 2200 /
plan_todo 300（基本 done）/ others 同 T12 = 9980`，
`reason='final_delivery_summary'`，未触发 drop；交付物 card：PR 标题 +
4 overlay 状态（all clean）+ diff artifact 下载 + markdown 预览。

---

## §4 发现的别扭点（Frictions，按严重度排）

### F1 · T5 manifest vs projection 层一致性（`notable`）
- **Ref**: T5 / §Revision Reconciliation L443 + §Workspace Reachability L2687
- **Observation**: iPad 冷启拿到 T3 manifest_id cached row，manifest 里
  `tool_schemas` zone 仍是 hybrid 超集（含 `edge_fs_write`），但 UI
  展示可用工具是 cloud 子集。Context 面板若直接渲染 manifest JSON，
  会向用户展示"可用 edge_fs_write"，但点击被 403。
- **Why friction**: manifest 是"上一次 LLM 看到什么"的快照，不是"下一次
  LLM 会看到什么"。跨 authority 切换二者语义分叉，v0.2 未规定"展示层
  应否 overlay projection 裁剪结果在 cached manifest 上"。实现者要么
  双写新 manifest（浪费，无 LLM 调用），要么展示层 join（隐式规则易漏）。
- **Severity**: `notable`

### F2 · T7 workspace authority 持久化切换时机（`notable`）
- **Ref**: T5→T7 / §Workspace Reachability L2697–2700
- **Observation**: G11 规 "cloud relay 转发 edge tool call 前 200ms
  探活失败 → 写 projection + emit `workspace_reachability_changed`"，
  但 iPad 段 T5–T6 无 edge tool call 仅纯读。这两回合是否要把
  `authority` 持久化切 cloud？S03 倾向 T7 真 turn 才落库，v0.2 未说
  "projection-only 降级"的边界。
- **Why friction**: 若 T5 就落库，换设备产生多对 from/to event 噪声大；
  若 T7 才落库，iPad 纯读阶段 `workspace_state` 行返 hybrid 但 UI 显
  cloud，逻辑分裂。
- **Severity**: `notable`

### F3 · T10 `unknown_device` 过期与 Chromebook 本地 storage 不同步（接近 blocking）
- **Ref**: T10 / §Revision Reconciliation L488–497
- **Observation**: G13 定义 `unknown_device` "reads denied or restricted"，
  但未规 `lease.expires_at` 自动过期时是否 emit SSE 到 user 其他活跃
  tab；更严重：Chromebook cookie / access_token 仍有效 15 分钟后，
  server 401 靠 fetch error 检测，**没主动 push 的 `device_revoked`
  事件**。
- **Why friction**: S03 F2 点名"未显式 logout 导致 session 泄露"；
  G13 revoke API 手动，lease 被动过期，二者合只保 server 拒绝，不能
  主动清 Chromebook 本地 storage。需"server 检测 lease expired 后
  通过 SSE push device_revoked → 前端清 storage"——v0.2 无描述。
- **Severity**: `notable`（趋近 `blocking`，涉安全）

### F4 · T11 `revision_hash` 含 `device_fingerprint` 的跨设备判等（`notable`）
- **Ref**: T11 / §Revision Reconciliation L446–451
- **Observation**: G13 L446 把 `device_fingerprint` 写入 `revision_hash`。
  客户端本地存 MacBook T4 hash 基于 `fp_mb_XX`，iPad T5–T7 写入时
  基于 `fp_ipad_YY`；MacBook T11 拿 server 返回 hash 必然异于本地。
- **Why friction**: L449 说 "full hash detects device-specific rollback,
  stale workspace state, or corrupted local cache"，但未规定
  **monotonic_id 单调 + hash 不等** 时如何决策。实现者可能把"hash 不等"
  当 corruption 触发 gap full reset，跨设备永远无法 delta，A1 的 token
  节省效果崩溃。
- **Severity**: `notable`

### F5 · T12 `stable_prefix_hash` 依赖 `active_skill.version_id` 稳定（`minor` 潜在）
- **Ref**: T3→T12 / §Personal Skill Activation L2130–2140
- **Observation**: A4 假设 MacBook T3 和 T12 的 `stable_prefix_hash`
  一致。G16 L2135 `version_id` 冻结支持不漂移。但若 Alice iPad 期间
  手动升级 skill（改 `active_skill.payload.version_id`），T12 skills
  zone 变 → cache miss。
- **Why friction**: skill 升级路径与 prompt cache 命中路径冲突。G16
  freeze 是对的，但缺"升级后下一回合 cache miss 的成本"告警——实现
  者可能默认 auto-update 致 A4 沉默失败。
- **Severity**: `minor`

### F6 · T9 Chromebook incognito page_hash 无处持久化（`minor`）
- **Ref**: T9 / §Web Event Watermark Atomicity L541–545
- **Observation**: G19 L541 提 `BroadcastChannel` 多 tab 同步，incognito
  禁用 IndexedDB 也无共享 storage。Chromebook 段快速滚 2 次 page_hash
  revalidate 都走网络。
- **Why friction**: 非关键路径，但 4G 下每次 ~200ms 延迟累加顿挫。
  workaround 内存 Map 缓存 tab 关即丢——G19 未规内存层行为。
- **Severity**: `minor`

### F7 · T11 edge bridge reattach 的 `device_fingerprint` 命名歧义（`notable`）
- **Ref**: T11 / §Workspace Reachability L2702–2709
- **Observation**: G11 L2704 "Edge bridge presents stable `edge_bridge_id`,
  `device_fingerprint`, user auth" — 这里的 `device_fingerprint` 是
  edge bridge desktop app 指纹还是浏览器 session 指纹？同一 MacBook
  可能有 `fp_mb_XX`（浏览器）和 `fp_edge_mb_XX`（edge app）。L2706
  "Server compares against `workspace_state` and `session_device_leases`"
  比对哪条？
- **Why friction**: 若用浏览器 fp 对账，edge desktop app 升级换 fp 时
  reattach 失败；若用 edge app fp，`session_device_leases` 需两类记录
  （browser-lease + edge-lease）。G13 表结构只一个 `device_id` 无区分
  字段。
- **Severity**: `notable`

---

## §5 发现的新 gap 候选

### Gap 候选 A · workspace authority 持久化切换的触发时机未定义
- **title**: Workspace authority persistence trigger boundary undefined
- **triggered_by_turn**: T5, T7, T11
- **related_v0.2_section**: §Workspace Reachability L2687–2700
- **severity**: medium
- **rationale**: G11 定义 `edge_status` 四态，但 `authority` 持久化
  **何时**切换没规。纯读路径（T5）、认证路径（T8）、心跳路径（T4）
  都可能变 projection 不落库，客户端看到 authority 与 server 存储不
  一致；T11 又要求立即切回 hybrid，推导规则隐晦。
- **proposed_fix**: 加 "Authority persistence policy"：(a) projection
  始终从 edge_status+policy derive；(b) 持久化切换只在下一次 LLM turn
  启动前或用户显式切换触发；(c) 写 `state_item_events` 记 `trigger_reason`。

### Gap 候选 B · lease 自动过期 vs 显式 revoke 的 SSE 对等性
- **title**: Auto-expiry vs explicit revoke — SSE event parity undefined
- **triggered_by_turn**: T10
- **related_v0.2_section**: §Revision Reconciliation L488–497
- **severity**: high（安全）
- **rationale**: G13 revoke API 主动，lease expires_at 被动过期没定义
  是否走同路径 emit `device_revoked` 事件。直接影响 T10 未 logout 场景：
  仅靠本地 cookie 失效无法清 localStorage，必须 server push。
- **proposed_fix**: 定义 `session_device_leases` 状态机
  `active → expired | revoked`，二者都 emit `device_lease_ended
  {reason, device_id}` 到 user 级 broadcast；前端收到清 IndexedDB +
  localStorage + 关 SSE。

### Gap 候选 C · `revision_hash` 跨设备判等语义
- **title**: Cross-device revision_hash compare: mismatch vs corruption
- **triggered_by_turn**: T11
- **related_v0.2_section**: §Revision Reconciliation L444–451
- **severity**: medium
- **rationale**: G13 L446 hash 含 `device_fingerprint`，L449 只说
  hash 用于检测 rollback/corruption。跨设备 A 写 B 读必然不等但非
  corruption，需区分 "cross-device legitimate mismatch" vs
  "same-device stale/corrupt"。
- **proposed_fix**: hash 输入改为"server 权威记录的
  last_writer_fingerprint"；客户端对比：monotonic_id 递增 + hash 等
  → pure delta；递增 + hash 不等 + last_writer_fingerprint ≠ self →
  cross-device delta（接受）；其他按现行 gap reset 或 409。

### Gap 候选 D · edge bridge fingerprint 与浏览器 fingerprint 命名冲突
- **title**: Edge bridge fingerprint namespace overlaps browser lease
- **triggered_by_turn**: T11
- **related_v0.2_section**: §Workspace Reachability L2702 + §Revision L458
- **severity**: medium
- **rationale**: 两处都用 `device_fingerprint` 语义不同；
  `session_device_leases.device_id` 无法区分 edge-app-lease 与
  browser-lease，T11 reattach 对账时 join 错表。
- **proposed_fix**: `session_device_leases` 增 `device_kind
  ENUM('browser','edge_bridge','mobile_app','cli')`；reattach API 显式
  声明 `device_kind='edge_bridge'`；edge_bridge 的 trust_level 默认
  trusted（已走 OS 级 auth）。

---

## §6 Gap 组合效应检验

### 组合 A · G13 + G11 — MacBook T11 回归联动
- **组合**: G13（device lease）+ G11（reachability）
- **运行时坑**: T11 浏览器 session 恢复 + edge bridge reattach 同时。
  G13 要求 `updated_at` + hash 重算；G11 要求 reattach 对账
  `device_fingerprint`。异步执行时浏览器先拿 delta（edge 还 offline），
  0.5s 后 edge 上线再 emit projection change，Context 面板
  tool_schemas 需再刷。manifest 可能在 T12 turn 启动前 0.2s 内变化，
  `stable_prefix_hash` 计算窗口狭窄。
- **v0.2 覆盖**: partial — G11 L2711 有 reattach API，G13 L454 有 lease
  表，但二者无顺序契约
- **建议**: 新增 "Browser session + edge reattach coordination"：
  (a) edge reattach 必须在 browser hot path 3 条查询完成后、LLM turn
  启动前窗口内处理；(b) reattach 成功不立即 rebuild manifest 等下次
  turn；(c) 客户端订阅 `workspace_reachability_changed`，UI layer
  更新 tool badge 不触发 manifest 重建。

### 组合 B · G13 + G19 — MacBook T4 合盖 / T11 恢复
- **组合**: G13（device lease）+ G19（watermark atomicity）
- **运行时坑**: T4 `beforeunload` 内 G19 IndexedDB 事务 flush
  watermark；同时 G13 `last_monotonic_id` 需更新 server。若
  `beforeunload` 发 `sendBeacon` 而 lease 更新走普通 fetch，lease 更新
  可能失败（unload 期间 fetch cancel）。T11 恢复 server 看 lease
  `last_monotonic_id=11`（stale），client IndexedDB 是 12（G19 保证
  durable）— server 若以 `last_monotonic_id` 算 delta 基准，从 11 开始
  重发一条 event；G19 L521 "skip if event_idx <= current_watermark
  and event_hash matches" 救场，但需 event_hash 一致才安全。
- **v0.2 覆盖**: partial — G19 L521 有幂等 skip，未规 lease watermark
  刷新失败的兜底
- **建议**: 明规 "G13 `last_monotonic_id` 仅审计字段，不作 delta 计算
  基准；server 永远以请求参数 `known_state_revision` 为准"。G13 L462
  `last_monotonic_id` 加注释 "observability only, not reconciliation
  source of truth"。

### 组合 C · G13 + G16 — iPad T5 step-up × skill activation
- **组合**: G13（trust_level）+ G16（skill activation）
- **运行时坑**: T5 iPad `trust_level=new_device`，G13 L488 规"写动作需
  step-up"。T7 首 turn 里 `active_skill` 冻结版本的 skill 源码被
  selector 加入 prompt，skill 自带 `allow_tool_calls`（云工具白名单）。
  step-up 确认是针对"user 发起 tool_call"还是"skill 自带 tool_call"？
  若只覆盖前者，skill 代为调 `cloud_fs_write` 绕过 new_device 限制。
- **v0.2 覆盖**: no — G13 step-up 只规 "write actions"，未分 user vs
  skill auto-invoke
- **建议**: 在 §Personal Skill Activation 明规 "skill-issued tool_calls
  inherit caller's trust_level；new_device session 下 skill 的 tool_call
  也需 step-up 或降级 dry_run"；或在 G13 step-up 规则列覆盖范围
  （user-input / skill / agent / delegate）。

---

## §7 T5 iPad 冷启 manifest token 账

**前提**: T5 本身**不触发 LLM turn**，故无新 manifest 写入。UI 会
`GET /context/latest` 命中 T3 manifest_id 并**在展示层**按 cloud
projection 裁剪。拆三账。

### 7.1 T5 UI 展示的 effective manifest（projection 层）

| Zone | T3 原值 | T5 projection 裁剪 | Cap (200k) | 说明 |
|---|---:|---:|---:|---|
| system_static | 1200 | 1200 | 稳定 | 无 workspace-specific |
| tool_schemas | 2400 | **1500** | 动态 | 剔 6 条 edge ≈ 900 tokens |
| skills | 700 | 700 | ≤1000 | G16 冻结 version_id |
| session_anchor | 300 | 300 | ≤500 | anchor 不受 authority 影响 |
| plan_todo | 700 | 700 | active phase | T3 写，iPad 只读 |
| recent_tail | 1100 | 1100 | 2–4 turn | 无 T4（无 LLM event） |
| tool_previews | 520 | 520 | 当前 patch | `session_tool_outputs` 取 |
| workspace | 260 | 260 | 双指纹 | 内容未变 |
| **小计** | **7180** | **6280** | — | 差 **900** |

effective = 6280 远低 200k 预算无 drop 风险。但这是展示 projection，
非 T7 真 turn manifest。T7 manifest 重新从 hot projection 构建，zone
会微调（anchor 被 T6 note 撑到 380；tool_previews 降 300）。

### 7.2 T5 网络 payload 账（A3 的 3 条有界查询）

| 查询 | 行数 | 估算 bytes | 索引 |
|---|---:|---:|---|
| `GET /state` | 19 | ~14 KB | `(user_id, session_id)` 唯一 |
| `GET /transcript?limit=50` | 45 | ~85 KB | `(session_id, item_seq DESC)` |
| `GET /chat/runs/run_k8s_01` | 1 | ~2 KB | 主键 |
| `GET /context/latest` | 1 | ~6 KB | `(session_id, created_at DESC)` |
| **合计** | — | **~107 KB** | 4G 1.2–2.0s RTT 内 |

对比朴素全量下发 ~3 MB（4G >15s）节省 ~**97%**（网络维度）。

### 7.3 T7 真 turn 的 manifest（与 T5 projection 对比）

T7 触发 LLM 重构 manifest（`reason='cross_device_cloud_turn'`）：
`system_static 1200 / tool_schemas 1500 / skills 700 / session_anchor
380（+T6 note）/ plan_todo 680 / recent_tail 900 / tool_previews 300 /
workspace 200 = 5860`。`stable_prefix_hash=px_cloud_v1`（与 T3
`px_hybrid_v1` 不同，tool_schemas 差 900 且内容哈希完全不同）；
`prompt_cache_key=pk_cloud_v1` 新键，iPad provider cache 首次 miss
成本 ~$0.0176 input（5860×$3/M）。Alice 后续 iPad 继续对话，
`px_cloud_v1` 可复用，T7' 命中 cache。

### 7.4 假设 iPad 切 small-window 模型（8k 窗口）

若 iPad 切 Claude Haiku 4 (16k) → `budget_v1_8k`（G10 L2410–2426）：

| Zone | cap | T7 实际 | 结果 |
|---|---:|---:|---|
| Session anchor | 200 | 380 | **超 180** → 裁 T6 note |
| Plan/todos | 400 | 680 | **超 280** → 保 active phase |
| Recent tail | 2000 (floor 1600) | 900 | OK |
| System + tool schemas | 3400 | 2700 | OK |
| Tool previews | 500 | 300 | OK |
| 其他 | — | 0 | — |
| **合计上限** | **8700** | **6060** post drop | drop_count=2 |

8k 预算 T7 触发 `dropped_count>0`（≥2 zone 超 cap）。S03 假设 200k
无此问题，但 G10 的 anchor/plan_todo cap 在真实移动会偏保守，需要
dynamic cap 或 per-turn 重估。

---

## §8 最终判断

**v0.2 可实现性**: ⚠️（基本可实现，4 处语义边界未明 + 1 处接近安全风险）

**顺畅度**: ⚠️ 有明显摩擦 — T5 projection vs manifest 一致（F1）、
T10 lease 自动过期与客户端 storage 清理（F3）、T11 revision_hash 跨
设备判等（F4）、edge bridge fingerprint 命名冲突（F7），均需实现者
自行约定二义性方能落地。

**一句话总结**: G13 + G11 + G19 合起来让 S03 机制上可达（hot path
有界 + delta 单调 + 设备信任分级），但"projection 层降级 vs 持久化
authority 切换（A）""lease 自动过期的 SSE 语义（B）""cross-device
revision_hash 判等（C）"三处语义空白需要再发 v0.2.1 补丁 + 新开 3–4
条 gap（B 定 high，A/C/D 定 medium）才能让实现者不踩坑。
