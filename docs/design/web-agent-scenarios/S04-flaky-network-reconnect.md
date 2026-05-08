# S04 — 断线续传：5 小时 run 中网络抖动 17 次

> Status: Draft v0.1
> Date: 2026-05-06
> Owner: astra runtime / web agent
> 关联设计文档: [../web-agent-session-state.md](../web-agent-session-state.md)
> 索引: [INDEX.md](INDEX.md)
> 被测 DAUT: **A1, A7, A8**（一等被测），次级接触 A2、A13

## 1. 场景概览

### 1.1 用户画像

- **角色**：陈老师，某省级数据中心 DBA，负责把一个老旧 MySQL 8.0 业务库（订单 + 物流，~1.4 亿行，320 GB）迁到公司新上的 MatrixOne HTAP 集群。
- **身份**：企业用户，`user_id=u_chen`。
- **设备**：MacBook Pro，Chrome 138。
- **环境**：早 07:00–12:00 高铁 G1234 南下出差，车内 5G + 车载 WiFi + 部分路段只有 4G，中间过 11 个隧道。

### 1.2 业务背景

迁移任务预估 5 小时，分 5 个阶段：

1. schema 反射（读 MySQL information_schema，生成 MatrixOne DDL 草稿）
2. DDL 校对 + **高危 DDL 审批** #1（drop legacy 分区）
3. 分批 SELECT + INSERT（每批 500k 行，共 280 批）
4. **DDL 审批** #2（创建全文/向量索引，耗资源大）
5. 对账校验（`SELECT COUNT(*)` + 抽样 checksum）

共 ~200 次 tool call（不含 heartbeat）。

### 1.3 一句话目标

> 让用户在 5 小时内 **只点 2 下审批按钮**，期间可以随意进隧道、切热点、关电脑盖、30 分钟不看、开新 tab，回来就能接着看，**不因为网络或服务端原因让 agent 重做任何一步**。

### 1.4 关键数值指标

| 指标 | 数值 |
| --- | --- |
| 总轮数（LLM rounds） | 42 |
| Tool call 数 | 203（180 SELECT/INSERT + 18 DDL/validate + 5 meta） |
| Run event 总数 | 约 560（含 started/tool_*/llm_round/approval_*/heartbeat/run_finished） |
| Session 持续时间 | 4h 38m |
| Artifact 数 | 8（DDL 稿、迁移报告、2 份 checksum 对账表、4 份 query plan） |
| 断连事件数 | **17** |
| 审批事件数 | 2 |
| 服务端 pod 重启次数 | 1（恰好撞上发版滚动升级） |
| Tab 数峰值 | 2（同一 session） |

### 1.5 17 次断连分布（为真实感列出）

| # | 时刻 | 类型 | 时长 | 期间服务端状态 |
| --- | --- | --- | --- | --- |
| D01 | 07:08 | 隧道 | 15 s | running |
| D02 | 07:22 | 4G/5G 切换 | 4 s | running |
| D03 | 07:35 | 隧道 | 22 s | running |
| D04 | 07:41 | WiFi 抖动 | 20 s | **waiting_for_user**（审批 #1） |
| D05 | 07:58 | 隧道 | 8 s | running |
| D06 | 08:12 | 车顶过桥 | 3 s | running（heartbeat 误判候选） |
| D07 | 08:30 | 5G→4G | **4 min 10 s** | running |
| D08 | 08:55 | 隧道 | 18 s | running |
| D09 | 09:03 | WiFi 抖动 | 11 s | running |
| D10 | 09:10 | 用户主动**开新 tab**（非刷新） | —（双连） | running |
| D11 | 09:20 | 旧 tab 关电脑盖 | 持续 | running（仅 tab#2 在） |
| D12 | 09:48 | 隧道 | 12 s | running |
| D13 | 10:00 | **pod 重启（发版）+ 用户盖笔记本** | **30 min** | running→(gap)→running（新 pod） |
| D14 | 10:42 | 5G 抖动 | 9 s | **waiting_for_user**（审批 #2） |
| D15 | 11:20 | 隧道 | 25 s | running |
| D16 | 11:30 | 3 s 闪断（heartbeat 窗口内） | 3 s | running |
| D17 | 11:33 | WiFi 抖动 | 7 s | running |

---

## 2. 被测设计假设

| DAUT | 解释 | 本场景如何压测 |
| --- | --- | --- |
| **A1** | MatrixOne 是 web session 唯一权威；浏览器只发 `session_id` + 增量 watermark，服务端重建 context | D10 双 tab、D11 旧 tab 关闭、D13 pod 重启后新 tab 恢复 —— 任何一端的 IndexedDB 都不能污染服务端；`run_event_high_watermark` 每一步必须严格按 `event_idx` 推进，跳号必须检测并补 |
| **A7** | Run 是 durable（`agent_runs` + `agent_run_events`），server 重启 / SSE 断线可按 `event_idx` 续接 | 17 次断连 + 1 次 pod 重启；重连走 `GET /chat/runs/{run_id}/stream?last_index=N` 只拉 N+1 之后；pod 重启后必须从 **DatabaseRunStateStore** 恢复 run + 续发事件（直戳设计文档 Important Gap #1：当前 `state_builder.rs` 仍然 wire `InMemoryRunStateStore`） |
| **A8** | `waiting_for_user` 是 durable run 状态，审批/问询是 durable event | 2 次 DDL 审批。D04 审批中断网 20 s、D13 审批前 pod 重启、D14 审批中 5G 抖动 —— 审批请求写 event_idx 后 run 状态必须立刻落 `agent_runs.status='waiting'`、`waiting_for='tool_approval:<approval_id>'`；30 分钟后客户端回来 `GET /chat/runs/{run_id}` 仍然看到 `waiting_for_user` |
| A2（次） | 审计/投影/manifest 三表分离 | 17 次断连都不应触发 `context_manifests` 重写；manifest 和 run event 是两条独立时间线 |
| A13（次） | 父子 delegation | 本 migration 内部没有子 delegation，但 `parent_run_id=NULL` 的行为要和 S09/S10 保持一致 |

---

## 3. 会话时间线

为可读，每个 T 都给出：**用户镜头 → 客户端动作 → 服务端动作（DB 与 event_idx） → LLM 可见/不可见内容**。

### T01 — 07:05：启动 run

**用户镜头**：陈老师打开 laptop，上周在办公室起草过一个 `mysql_to_mo_migration.md` plan，今天挂 plan 点「继续」。

**客户端 → 服务端**

```http
POST /chat/stream HTTP/1.1
Content-Type: application/json
Authorization: Bearer <u_chen's token>

{
  "session_id": "s_mig_2026_05_06",
  "message": "按 plan 开始执行 legacy_orders → mo_orders 迁移，每批 500k 行，遇 DDL 暂停问我。",
  "client_watermarks": {
    "state_revision": 11,
    "transcript_high_watermark": 48,
    "run_event_high_watermark": 0
  }
}
```

**服务端**（按 `web-agent-session-state.md` §"Durable Runs" 定义的契约）

```sql
INSERT INTO agent_runs
  (run_id, user_id, session_id, status, execution_mode, trigger_type,
   request_id, trace_id, created_at, updated_at)
VALUES
  ('r_mig_g1234', 'u_chen', 's_mig_2026_05_06',
   'running', 'cloud', 'user_message',
   'req_0001', 'trace_g1234', NOW(), NOW());

INSERT INTO agent_run_events
  (run_id, event_idx, user_id, session_id, event_type, payload_json, created_at)
VALUES
  ('r_mig_g1234', 0, 'u_chen', 's_mig_2026_05_06',
   'run_started',
   JSON_OBJECT('session_id','s_mig_2026_05_06','model','gpt-4o-long'),
   NOW());
```

写入后，`build_initial_context()` 构造第一个 `context_manifest`，随即挂 SSE。

**LLM 能看到**：session anchor（迁移目标、白名单表、每批 500k、DDL 需审批）、plan v3、recent tail 最近 2 轮、工具 schema（`mysql.query`、`matrixone.exec`、`validate.checksum`）。

**LLM 不能看到**：浏览器 IndexedDB 缓存里的旧 tool preview、state_revision=11 本身（只是协商 delta 用的传输层字段）。

**写入事件**：`event_idx = 0..3` 分别是 `run_started / llm_round_started / tool_call_start(mysql.query) / tool_call_end`。

### T02 — 07:08：D01 第 1 次隧道断网，15 秒

**用户镜头**：G1234 进韩庄隧道，Chrome 标签页顶部「离线」提示条出现。

**客户端**：

- `use-chat-stream.ts` 的 `EventSource.onerror` 触发。
- 不清空 UI，不回放；记录 `lastIndex = 3`。
- 进入「指数退避 + 重连」循环：500 ms、1 s、2 s…

**服务端**：完全不知道客户端断了。`tokio::mpsc` 对端 receiver 被 drop，但 run engine 继续推进，把后续 `event_idx=4..9` 写入 `agent_run_events` 并 fanout 到 SSE broadcaster（没订阅者就直接落库）。

**关键契约**：这是 A7 的核心 —— **事件生产不依赖订阅者存在**。违反这一条，断线等于丢事件。

### T03 — 07:08+15s：出隧道，自动重连

**客户端 → 服务端**：

```http
GET /chat/runs/r_mig_g1234/stream?last_index=3 HTTP/1.1
Accept: text/event-stream
Authorization: Bearer <u_chen's token>
Last-Event-ID: 3
```

**服务端逻辑**（期望实现）：

```text
1. SELECT status, waiting_for FROM agent_runs WHERE run_id = ?
   → ('running', NULL)
2. SELECT event_idx, event_type, payload_json
   FROM agent_run_events
   WHERE run_id = ? AND event_idx > 3
   ORDER BY event_idx ASC
   LIMIT 1000
3. 先把历史 4..9 作为 SSE 「replay」阶段推给客户端，每条
   id: <event_idx>
   event: <event_type>
   data: <payload>
4. 追完后切换为 live broadcast，接着 fanout event_idx=10..
```

**客户端**：收到 id=4..9 的 replay，在 IndexedDB `run_event_high_watermark` 原子推进到 9，UI 不抖。

**代价对比（节省视角）**：

- 若没有 durable run：客户端需要重新 `POST /chat/stream` + 把 5 秒内已跑的 6 个 tool_call 重放一遍，LLM 必须重新推理一次「下一步是什么」。估算：重放 1 轮 LLM round ≈ 32k prompt tokens + 1k completion，wall-clock ~25 s；**且第二次执行 INSERT 会幂等失败 / 脏写**。
- 有 durable run：0 token 重放，0 tool 重做，HTTP round-trip ~80 ms。

### T04 — 07:40：审批 #1，DDL 高危

**用户镜头**：agent 已经跑完 schema 反射和前 22 批 INSERT，现在要执行：

```sql
DROP TABLE legacy_orders_2020_q1, legacy_orders_2020_q2;
```

UI 中央弹出 approval card：「以下 DDL 不可逆，请确认 [批准 / 拒绝 / 让我看执行计划]」。

**服务端动作**：

```sql
INSERT INTO agent_run_events
  (run_id, event_idx, event_type, payload_json, ...)
VALUES
  ('r_mig_g1234', 46,
   'approval_requested',
   JSON_OBJECT(
     'approval_id','appr_ddl_0001',
     'tool_name','matrixone.exec',
     'danger','drop_table',
     'sql','DROP TABLE legacy_orders_2020_q1, legacy_orders_2020_q2',
     'issued_at','2026-05-06T07:40:12Z',
     'expires_at','2026-05-06T09:40:12Z'
   ),
   NOW());

UPDATE agent_runs
SET status = 'waiting',
    waiting_for = 'tool_approval:appr_ddl_0001',
    updated_at = NOW()
WHERE run_id = 'r_mig_g1234';
```

同时写 `session_state_items(category='approval_state', item_key='appr_ddl_0001', status='pending')`。

SSE 推：`id: 46\nevent: approval_requested\n`。

**LLM 暂停推理**。`RunEngine::advance_run` 进入 `await input`。

### T05 — 07:41：D04 审批中 WiFi 抖动 20s

**用户镜头**：陈老师刚要点「让我看执行计划」，Chrome 弹「离线」。

**客户端**：`EventSource` 断开。UI 还停在 approval card 上（因为 React state 没动），但背景颜色从绿变橙（离线指示）。

**服务端**：**完全不动**。`agent_runs.status='waiting'` 保持，`waiting_for='tool_approval:appr_ddl_0001'` 保持。`expires_at` 计时照跑（见失败模式 F3）。

### T06 — 07:41:20：重连，用户批准

**客户端 → 服务端**（重连）：

```http
GET /chat/runs/r_mig_g1234/stream?last_index=46 HTTP/1.1
```

服务端 replay 返回 **0 条新事件**（因为 waiting 期间不产生事件，除非心跳 —— 见 F5 讨论），但在 SSE 打开瞬间发一条 `event: run_state_sync` 告诉客户端当前 `status=waiting, waiting_for=appr_ddl_0001`，让 UI 复活 approval card（即使 React 本地 state 在断线前还在，也要以服务端为准 —— A1）。

**用户点「批准」**：

```http
POST /chat/runs/r_mig_g1234/input HTTP/1.1
Content-Type: application/json

{
  "kind": "approval_response",
  "approval_id": "appr_ddl_0001",
  "decision": "approve",
  "comment": "执行前 snapshot 已经拉过",
  "idempotency_key": "u_chen:appr_ddl_0001:approve"
}
```

**服务端**：

```sql
INSERT INTO agent_run_events
  (run_id, event_idx, event_type, payload_json, ...)
VALUES
  ('r_mig_g1234', 47,
   'approval_granted',
   JSON_OBJECT(
     'approval_id','appr_ddl_0001',
     'decided_by','u_chen',
     'decided_at','2026-05-06T07:41:35Z'
   ),
   NOW());

UPDATE agent_runs
SET status = 'running', waiting_for = NULL, updated_at = NOW()
WHERE run_id = 'r_mig_g1234' AND status = 'waiting';
-- 注意 WHERE 上的 status guard：防止 D07 路径上的重复写

UPDATE session_state_items
SET status = 'resolved', payload_json = JSON_SET(payload_json, '$.decision', 'approve')
WHERE item_key = 'appr_ddl_0001' AND session_id = 's_mig_2026_05_06';
```

`RunEngine::advance_run` 拿到 input，继续推 `event_idx=48..` 执行 DDL。

**幂等关键**：POST /input 的 `idempotency_key` 是表示「同一次人类意图」的 dedup key（见 F6）。服务端对 `(run_id, approval_id, decision)` 去重。

### T07 — 08:30：D07 最长的前半段，5G→4G 切换 4 分钟

**用户镜头**：到宿州站附近信号降级，Chrome 提示离线。

**服务端**：这 4 分钟内持续跑，event_idx 从 47 推进到 **98**（50 个事件：约 20 个 tool_call_start/end、10 个 llm_round 中间步、12 个 progress、8 个 heartbeat）。

**客户端**：按原退避策略，2s、4s、8s…（cap 30s）无限重连。每次都收 net::ERR_INTERNET_DISCONNECTED。

### T08 — 08:34：重连补齐 51 个事件

**客户端**：`lastIndex=46`（注意：**不是 47**。原因见下文）。

> 为什么是 46 不是 47？IndexedDB 写 `run_event_high_watermark=47` 的那次 commit 恰好被 D04 的断线打断（IndexedDB 异步 `transaction.oncomplete` 没跑完页就卡住了）。这正是 F2 要压测的失败模式。

**服务端**：

```sql
SELECT event_idx, event_type, payload_json FROM agent_run_events
WHERE run_id = 'r_mig_g1234' AND event_idx > 46
ORDER BY event_idx ASC LIMIT 1000;
-- 返回 52 条：47..98
```

SSE 顺序推送 47..98，客户端重放时要识别：

- `event_idx=47` 是 `approval_granted`，客户端已在本地 state 里标记过 —— 看到 replay 里的 47 必须做 **应用层 dedup**（按 `approval_id` 而不是 `event_idx`）。
- `event_idx=48..98` 是新增，正常渲染 tool timeline。

**A1 严格推进**：`run_event_high_watermark` 必须顺序 47→48→…→98，中间不能跳到 98 然后回补。如果服务端出 bug 先发 98 再发中间的，客户端必须检测 gap 并**重新请求** `?last_index=47`。

```ts
// 期望的客户端伪码
onEvent(e: SSEEvent) {
  const idx = e.idx;
  if (idx !== watermark + 1) {
    // gap detected — reconnect with current watermark
    reconnect(watermark);
    return;
  }
  applyEvent(e);
  watermark = idx;
}
```

### T09 — 09:10：D10 用户开新 tab（不是刷新）

**用户镜头**：陈老师想一边看 agent 进度，一边在另一 tab 打开 MatrixOne 的 query UI 手动 `SELECT COUNT(*)` 验证。他**另起新 tab** 打开 `https://astra.example.com/workspace?session_id=s_mig_2026_05_06`。

**客户端**（tab#2 冷启动）：

```http
GET /sessions/s_mig_2026_05_06/state HTTP/1.1
→ 200 OK
{
  "session": {...},
  "active_run": {
    "run_id": "r_mig_g1234",
    "status": "running",
    "waiting_for": null,
    "run_event_high_watermark": 131
  },
  "plan": {...},
  "todos": [...],
  "latest_context_manifest": {...}
}
```

然后 tab#2 自己的 IndexedDB 是空的（新 origin 同名但 tab 间 IndexedDB 共享，实际会读到 tab#1 的缓存 —— 但如果是隐身模式或不同 profile 则空）。

**假设最坏情况：tab#2 独立 IndexedDB，`run_event_high_watermark=0`**。则：

```http
GET /chat/runs/r_mig_g1234/stream?last_index=0 HTTP/1.1
```

tab#2 将**全量 replay** 131 个事件。成本：

- 服务端 1 次索引范围扫描，~5 ms（`(run_id, event_idx)` UNIQUE KEY 覆盖）；
- 网络 payload ~800 KB gzip；
- 客户端渲染 131 条 ~120 ms。

**同时 tab#1 还挂着 SSE**。服务端现在有两个 subscriber 订阅同一 `run_id`。

**服务端 fanout 策略**（设计需要明确的，见反馈 §8）：

```rust
// 期望
broadcaster: tokio::sync::broadcast::Sender<RunEvent>
// 两个 subscriber 都收到新 event_idx=132..
```

使用 `tokio::sync::broadcast`（多 receiver 单 sender）即可；`event_idx` 仍由 DB 的 `UNIQUE KEY (run_id, event_idx)` 串行化。

**A1 验证**：两个 tab 的 UI 最终状态必须一致；浏览器任一端都不能决定 run 状态。

### T10 — 09:20：D11 旧 tab 关电脑盖

**用户镜头**：笔记本盖上，tab#1 SSE 断。tab#2 继续在桌面显示器上工作。

**服务端**：从两 subscriber 变一 subscriber，不影响事件生产。

### T11 — 10:00：D13 pod 重启 + 用户离线 30 分钟

**这是全场景最硬的压测点。**

**10:00:03** 运维开始滚动发版。pod `astra-runtime-6fdb`（正跑 run `r_mig_g1234`）被 SIGTERM。

**期望的服务端 shutdown 行为**：

```text
1. 停止接受新连接
2. 现有 SSE 断开（客户端 tab#2 会看到 EOF）
3. RunEngine flush pending writes:
   - 最后一批 event_idx 写 DB（假设写到 187）
   - agent_runs.checkpoint_json 更新 {last_batch_id: 147, last_ok_at: ...}
4. agent_runs.status 保持 'running'（不是 failed）
   —— 这点和当前 find_running_runs() 在恢复流程里「标 failed」的语义有冲突，见 F7
```

**10:00:12** 新 pod `astra-runtime-a81e` 起来。启动时：

```rust
// 期望
async fn on_startup(store: Arc<dyn RunStateStore>) {
    let orphans = store.find_running_runs().await?;
    for run in orphans {
        // 不是标 failed，而是 resume
        run_engine.resume_from_checkpoint(&run).await?;
    }
}
```

**10:00:15** 新 pod 加载 `run_id=r_mig_g1234`，读 `checkpoint_json.last_batch_id=147`，重新从第 148 批 SELECT 开始。第一个新事件写入：

```sql
INSERT INTO agent_run_events
  (run_id, event_idx, event_type, payload_json, ...)
VALUES
  ('r_mig_g1234', 188,
   'run_resumed_after_restart',
   JSON_OBJECT('new_pod','astra-runtime-a81e','from_checkpoint',147),
   NOW());
```

注意 `event_idx=188` **严格延续** shutdown 前的 187。靠 DB `UNIQUE KEY (run_id, event_idx)`：

```sql
-- 伪代码
SELECT COALESCE(MAX(event_idx), -1) + 1 AS next_idx
FROM agent_run_events WHERE run_id = ?;
-- 插入时用 INSERT ... ON DUPLICATE KEY → retry
```

用户：10:00–10:30 完全离线（高铁进秦岭段 + 电脑盖着）。

### T12 — 10:30：用户打开 laptop，恢复

**用户镜头**：笔记本唤醒，tab#2 Chrome 刚刚 resume，SSE 自动重连。

```http
GET /chat/runs/r_mig_g1234/stream?last_index=131 HTTP/1.1
```

（131 是 T09 tab#2 冷启后接到的第一个 watermark 加上这 30 分钟前积累的，假设 tab#2 最后一次成功写 watermark 是 131。）

**服务端**：replay event_idx=132..205（74 个事件，跨越 D13 的 pod 重启和中间的 `run_resumed_after_restart` 事件）。

客户端 timeline UI 渲染出一条「服务端重启了，已自动续接」的系统消息（来自 `event_type=run_resumed_after_restart` 的 payload）。

另一边陈老师再 `GET /chat/runs/r_mig_g1234`：

```json
{
  "run_id": "r_mig_g1234",
  "status": "running",
  "waiting_for": null,
  "events_count": 205,
  "current_activity": {
    "tool_name": "matrixone.exec",
    "batch_id": 156,
    "rows_written_so_far": 78000000
  }
}
```

**这是 A7 最核心的证明点**：用户离开 30 分钟，服务端经历了一次 pod 重启，回来 agent 还在跑，不需要重新描述任务、不需要重新审批、不需要重跑任何一批 INSERT。

### T13 — 10:45：审批 #2

类似 T04，但 D14 此时正好 5G 抖动 9 秒。因为 waiting 不产生新事件（只产生 idle heartbeat），重连几乎无感。

approval_id = `appr_ddl_0002`, `tool_name=matrixone.exec`, `sql='CREATE FULLTEXT INDEX idx_ft_desc ON mo_orders(description)'`, `expires_at=2026-05-06T12:45:00Z`。

`event_idx=218 approval_requested` → `agent_runs.status='waiting', waiting_for='tool_approval:appr_ddl_0002'`。

### T14 — 10:46：批准 #2

```http
POST /chat/runs/r_mig_g1234/input
{ "kind":"approval_response", "approval_id":"appr_ddl_0002", "decision":"approve",
  "idempotency_key":"u_chen:appr_ddl_0002:approve" }
```

`event_idx=219 approval_granted` → `status='running'`。

### T15 — 11:30：D16 — heartbeat 窗口内的 3 秒闪断

**场景**：最后对账阶段，agent 正在跑 `SELECT COUNT(*)` 大查询（预期 45 秒返回）。中间 3 秒客户端 TCP RST（高铁信号塔切换）。客户端的 SSE heartbeat 阈值设为 20 秒无事件触发探活。3 秒远小于 20 秒。

**正确行为**：客户端不需要重连。TCP 层会重建，HTTP 长连接在内核缓冲里重组，SSE 事件流不中断。

**容易出错的行为**（F5）：如果客户端用的不是「心跳阈值」而是「socket error 就断」，或者用 React-level `onerror` 立刻 reconnect，这个 3 秒闪断就会导致一次**不必要的** reconnect —— 后果是一次多余的 `GET /chat/runs/.../stream?last_index=N`，按 `event_idx > N` 查询，极大概率返回 0 条新事件，浪费一次 HTTP 握手。

### T16 — 11:33：D17 —  WiFi 抖动 7 秒（类似 T03，略）

### T17 — 11:35：run_finished

```sql
INSERT INTO agent_run_events
  (run_id, event_idx, event_type, payload_json, ...)
VALUES
  ('r_mig_g1234', 561,
   'run_finished',
   JSON_OBJECT(
     'status','completed',
     'rows_total',138245119,
     'rows_migrated',138245119,
     'checksum_mismatch',0,
     'wall_clock_seconds',16740,
     'total_prompt_tokens',482910,
     'total_completion_tokens',47120,
     'total_tool_calls',203
   ),
   NOW());

UPDATE agent_runs
SET status = 'completed', updated_at = NOW(),
    total_prompt_tokens = 482910,
    total_completion_tokens = 47120,
    total_tool_calls = 203
WHERE run_id = 'r_mig_g1234';
```

---

## 4. 上下文压力点

| 压力点 | 本场景特有 | 设计需要扛住什么 |
| --- | --- | --- |
| **事件序列单调性** | 17 次断连 + 1 次 pod 重启 + 双 tab | `event_idx` 在 `(run_id, event_idx)` UNIQUE KEY 上全局单调；任何 replay 必须保证顺序；gap 必须可检测 |
| **waiting 状态持久化** | 审批中间断网、pod 重启跨越审批前后 | `agent_runs.status='waiting'` + `waiting_for` 字段必须是 DB 真相，不是进程内存；恢复后 `RunEngine` 能识别「我应该停在 await input」而不是「重新推理」 |
| **订阅者解耦** | 双 tab 并发、pod 重启切换订阅 | 事件生产端不感知订阅者数量；订阅者来了就从 DB 拉 + 切 live broadcast |
| **客户端 watermark 原子性** | IndexedDB 写被断线打断 | 单 tab 内必须保证 `apply(event) + write(watermark)` 原子；不能出现「event 显示了但 watermark 没推进」→ 下次 reconnect 少拉 |
| **审批 token 寿命** | 审批 + 30 分钟离线 | `expires_at` 必须在 payload 里，客户端 UI 倒计时；到期服务端要自动发 `approval_expired` 事件推进 run（变 `failed` 或走降级） |
| **Heartbeat 语义** | 3 秒闪断不应触发 reconnect | 心跳阈值 vs error 触发必须区分清楚；设计文档需要明确 SSE 保持策略 |
| **idempotency** | 双 tab 都按了「批准」 | `POST /chat/runs/{id}/input` 必须接受 `idempotency_key`；服务端对 `(run_id, approval_id, decision)` 去重 |
| **Compaction 不能撞 run** | 5h 长 run 中间可能跨 compaction 阈值 | Compaction 必须识别 `active_run` 并跳过该 run 产生的 event/tool_output，或至少保留 `artifact_ref` |

---

## 5. 期望的 state projection 演化

### 5.1 `agent_runs`（单行演化）

| 时刻 | status | waiting_for | checkpoint_json（摘要） | total_tool_calls |
| --- | --- | --- | --- | --- |
| T01 07:05 | running | NULL | `{phase:'schema_refl'}` | 0 |
| T04 07:40 | waiting | `tool_approval:appr_ddl_0001` | `{phase:'ddl_gate_1'}` | 22 |
| T06 07:41 | running | NULL | `{phase:'ddl_gate_1',approved:true}` | 22 |
| T11 10:00:03 | running | NULL | `{phase:'bulk_insert',last_batch:147}` | 147 |
| T11 10:00:15（新 pod） | running | NULL | `{phase:'bulk_insert',last_batch:147,resumed_from_pod:'astra-runtime-6fdb'}` | 147 |
| T13 10:45 | waiting | `tool_approval:appr_ddl_0002` | `{phase:'ddl_gate_2'}` | 184 |
| T14 10:46 | running | NULL | `{phase:'ddl_gate_2',approved:true}` | 184 |
| T17 11:35 | completed | NULL | `{phase:'done'}` | 203 |

### 5.2 `agent_run_events`（关键里程碑）

| event_idx | event_type | 发生时刻 | 为什么重要 |
| --- | --- | --- | --- |
| 0 | run_started | T01 | run 的 anchor |
| 46 | approval_requested | T04 | A8 第 1 次 durable 审批 |
| 47 | approval_granted | T06 | 幂等键 `u_chen:appr_ddl_0001:approve` |
| 98 | tool_call_end (batch 34) | T08 前 | D07 最长断连期间在跑 |
| 131 | llm_round_end (batch 47) | T09 | tab#2 首次接入时的 watermark |
| 187 | tool_call_end (batch 72) | T11 shutdown 前 | pod 重启前最后一批落 DB |
| 188 | **run_resumed_after_restart** | T11 new pod | 全场景最重要的 event_type |
| 218 | approval_requested | T13 | A8 第 2 次 durable 审批 |
| 219 | approval_granted | T14 | |
| 561 | run_finished | T17 | 终局 |

### 5.3 `session_state_items`（关键 items）

```text
category=anchor           item_key=goal             status=active   (T01→T17 始终)
category=plan_state       item_key=migration_plan_v3 status=active   (T01→T17)
category=approval_state   item_key=appr_ddl_0001    status=pending→resolved (T04→T06)
category=approval_state   item_key=appr_ddl_0002    status=pending→resolved (T13→T14)
category=workspace_state  item_key=source_mysql     status=active
category=workspace_state  item_key=target_mo        status=active
category=tool_ref         item_key=batch_72_result  status=active   (pod 重启前落)
category=tool_ref         item_key=batch_148_result status=active   (新 pod 起后首条)
category=summary          item_key=progress_at_09:10 status=active  (tab#2 接入时的 snapshot)
```

### 5.4 `context_manifests`（抽样）

| manifest_id | turn_id | run_id | created_at | included zones | dropped |
| --- | --- | --- | --- | --- | --- |
| ctx_0001 | t_0001 | r_mig_g1234 | 07:05 | anchor/plan_todo/tool_schemas/workspace | 旧 2025 年 session 5 条 |
| ctx_0015 | t_0015 | r_mig_g1234 | 07:40 | +approval_state:pending | — |
| ctx_0033 | t_0033 | r_mig_g1234 | 10:00:18 | +summary(progress_87) | 旧 batch 1..60 的 tool_preview 全 drop |
| ctx_0041 | t_0041 | r_mig_g1234 | 11:30 | +tool_previews(last 3 batches) | — |

**关键**：manifest 数量 = LLM round 数（42），**不是** event 数（561）。17 次断连**不触发**任何新 manifest。A2 严格保持。

---

## 6. Token 节省估算（重放成本视角）

> 本场景不是节省 LLM token，而是节省**重放成本**。
> 「没有 durable run」= 每次断连都要 (1) 让 LLM 重新 reason「现在到哪了」(2) 重新 issue tool call (3) 可能重复写数据。

### 6.1 朴素方案（无 durable run）成本估算

假设：

- 平均每次断连后，客户端必须重新 POST /chat/stream 带上整段 transcript；
- LLM 重新推理「我处在哪」：1 个完整 round ≈ **32k prompt tokens + 1k completion tokens**（因为要重新消化 plan + 当前进度）；
- wall-clock 每次重放 ≈ 25 s；
- **Tool 侧副作用：不可幂等的 INSERT 可能重放**。

17 次断连 + 1 次 pod 重启 = 18 次 replay。

| 维度 | 朴素方案 | 本设计 | 节省 |
| --- | --- | --- | --- |
| 重放 LLM prompt tokens | 18 × 32,000 = **576,000** | 0 | **576,000 (100%)** |
| 重放 LLM completion tokens | 18 × 1,000 = **18,000** | 0 | **18,000 (100%)** |
| 重放 tool calls（理论） | 18 × 平均 11 个 tool = **198 次重放** | 0 | **198 次 (100%)** |
| 数据写入重放风险 | 180 批 INSERT × 10% 重放概率 ≈ **18 批脏写** | 0 | 无法用 token 表达，按「数据损坏事件数」计 **18→0** |
| 重放 wall-clock | 18 × 25 s = **450 s** = 7 min 30 s | 17 × ~80 ms + 1 × ~200 ms = **~1.6 s** | **448 s (99.6%)** |
| 审批 UX 损失 | 每次断连审批消失 → 用户要二次确认 → 至少 **6 次多余确认** | 0（状态持久） | **6 次 (100%)** |

### 6.2 本设计的额外成本（诚实交代）

| 成本项 | 量级 | 评价 |
| --- | --- | --- |
| `agent_run_events` 写入 | 561 行 × ~400 B = 220 KB | 一次性，可忽略 |
| DB 索引维护 | `(run_id, event_idx)` UNIQUE 1 个 + 3 个普通索引 | 560 次 insert × ~0.3 ms = 170 ms 总累计 |
| Replay SQL | 18 次范围扫 × 平均 30 行 × ~5 ms = 90 ms | |
| IndexedDB 写 watermark | 561 次 × ~2 ms = 1.1 s 分散 | |
| 额外网络流量 | 替代朴素方案省了 >600 MB prompt 上行 | 净收益巨大 |

### 6.3 一句话结论

> 这个场景下「durable run」不是省 LLM token 的优化，而是「数据迁移不出数据错误」的**正确性前提**。Token 数字是附带的，**真正节省的是 198 次 tool 重放带来的脏写风险**。

---

## 7. 潜在失败模式

按严重度排序。每条给出：**触发条件 → 可观测信号 → 根因 → 建议防线**。

### F1 — `event_idx` 在两个 pod 间冲突

**触发条件**：T11 pod 重启时，旧 pod shutdown 手柄 100 ms 内还没 flush；新 pod 起来后读 `MAX(event_idx)=187`，开始写 188；与此同时旧 pod 的 Tokio task 被 cancel 但有一条 event=188 的 SQL 在 connection pool 里排队，延迟 200 ms 后进到 DB。

**可观测信号**：

- MatrixOne 日志：`Duplicate entry '188' for key 'uq_run_event_idx'`；
- 客户端 SSE 某条 event 丢失（新 pod 的 188 被拒）或旧 pod 的 188 变成 189（如果实现里用了 retry + re-MAX）；
- Ops 告警：run 事件时间戳非单调。

**根因**：`next_idx = MAX(event_idx) + 1` 是 TOCTTOU race；`UNIQUE KEY` 虽然防脏写但不防 **event 语义**乱序。

**建议防线**：

1. 旧 pod shutdown 时 **hold 一个 advisory lock** `SELECT GET_LOCK('run:r_mig_g1234', 5)`，新 pod 起来时 `GET_LOCK` 等完才开始写；
2. 或者 `event_idx` 用 DB 侧原子自增（把 `event_idx` 从 `INT` 改成针对 run 的计数器行 `run_counters(run_id, next_event_idx)` + `UPDATE ... SET next_event_idx = next_event_idx + 1` 并 `RETURNING`）；
3. 测试用例：在 `DatabaseRunStateStore` integration 里模拟两个 writer 并发，断言至少一个收到明确错误且不污染序列。

### F2 — 浏览器 IndexedDB watermark 落后

**触发条件**：T08 场景，客户端已经应用了 event_idx=47，但 IndexedDB 的 `put(watermark, 47)` transaction 还在 onsuccess 之前，进程/tab/线程被挂起，下次 reconnect 带 `last_index=46`。

**可观测信号**：

- SSE replay 里收到一条已经应用过的 `approval_granted`（因为客户端按 idx 去重，不按语义 id 去重）；
- 或用户看到一次 UI 「approval card 闪回再消失」；
- 审批按钮被点了两次（F6 放大）。

**根因**：`apply_event` 和 `persist_watermark` 不是同一个 atomic transaction。

**建议防线**：

1. IndexedDB 里把 `apply_event` 的 state 更新和 `watermark` 更新放进同一 `transaction(['events','meta'], 'readwrite')`；
2. 客户端应用层 dedup key：对 `approval_*` 事件以 `approval_id+decision`、对 `tool_call_*` 以 `tool_call_id` 去重，不要只靠 `event_idx`；
3. 测试：Chrome DevTools 模拟「提交 IndexedDB 前 kill tab」验证下次启动 replay 安全。

### F3 — 审批 token 过期 + run 状态不一致

**触发条件**：T04 发的 `appr_ddl_0001`, `expires_at=09:40:12`；用户 D13 离线 30 分钟从 10:00→10:30；若第 2 次审批 `appr_ddl_0002` 的 `expires_at` 被配置为 1 小时并且发版期间服务端 clock skew（NTP 尚未 sync）导致 `expires_at` 提前到期。回来时用户点「批准」但服务端判定 `expired`。

**可观测信号**：

- `POST /chat/runs/{id}/input` 返回 409 Conflict 带 `error_code=approval_expired`；
- UI 上按钮已禁用但用户还在点；
- `agent_runs.status='waiting'` 却没人能推进它。

**根因**：`expires_at` 只是 payload 里的元信息，没有服务端 timer 自动把 run 从 `waiting` 推到 `failed` 或 `expired`；也没有「approval expiration」的 durable event 设计。

**建议防线**：

1. 后台 sweeper job 定时扫 `agent_runs.waiting_for LIKE 'tool_approval:%'` 且关联 `session_state_items.expires_at < NOW()` 的 run，写 `event_type=approval_expired` 自动推 run 到 `failed` 或 `cancelled`；
2. `expires_at` 要 generous（这类大任务 ≥ 4 小时）；
3. 设计文档 §"Interaction Semantics" 必须明确「approval 也有生命周期事件」，不能只写 `approval_requested`/`granted`/`denied`。

### F4 — Compaction 在 run 中间跑错

**触发条件**：假设后台 compaction 策略是「session 空闲 > 10 分钟则触发」。T11 期间 run 看起来「空闲」30 分钟（pod 重启 + 用户离线），compaction job 误以为 session 结束，扫 `conversation_log` 把早期 batch 1..60 的 tool output preview 压成一条 summary，把对应 `session_tool_outputs` 行标 `is_compacted=true`。新 pod resume 后，RunEngine 要读 `batch_60_result` 做 checksum 对账 —— 读到 `is_compacted=true` 或空 preview。

**可观测信号**：

- Resume 后第一个 tool_call 失败，`error_code=tool_ref_not_found`；
- `context_manifest` 显示 `dropped_count` 异常高；
- 业务侧对账报「checksum 对不上」但实际数据没问题。

**根因**：Compaction 只看 session 时钟，不看 `agent_runs.status IN ('running','waiting')`。

**建议防线**：

1. Compaction trigger 必须先 `SELECT run_id FROM agent_runs WHERE session_id = ? AND status IN ('running','waiting')`，**非空则跳过**；
2. 即使跑，也必须保留 `session_tool_outputs` 的 `artifact_ref` 和 `content_hash`（只去掉 prompt preview，底层数据不动） —— 这正是设计文档 §"Tool Result and Artifact References" 的意图，但需要 compaction 实现时显式遵守；
3. 测试：长 run 中手动触发 compaction，验证 run 续跑仍可读 artifact。

### F5 — SSE heartbeat 丢失 vs 真断连误判

**触发条件 A**：服务端在 tool 执行 2 分钟期间不发任何事件。客户端心跳阈值 30 秒。2 分钟后客户端误判死连、reconnect。

**触发条件 B**：T15 D16 的 3 秒 TCP RST，客户端的 `onerror` 立刻 reconnect 而不是等心跳窗口，产生一次多余的握手。

**可观测信号**：

- 服务端日志里一个 `run_id` 的 reconnect 次数远高于实际断连次数；
- APM 监控：长 tool call 期间 SSE 抖动率异常。

**根因**：SSE heartbeat 语义在设计文档里未明确：
- 服务端是否周期性发 `event: heartbeat`？频率？
- 客户端是靠 `onerror` 还是靠「N 秒没事件」判断？
- 3 秒 TCP 闪断是不是 `onerror` 触发的？

**建议防线**：

1. 服务端 SSE 强制每 15 秒发一条 `event: heartbeat\ndata: {}`；
2. 客户端判活：`now() - last_event_ts > 45s` 才算死连；`onerror` 只进入「重试态」但不立刻 reconnect；
3. 测试：60 秒无事件 + 定期心跳，验证客户端不 reconnect。

### F6 — Approval 幂等性：双 tab 双击

**触发条件**：T09 开始有两个 tab 订阅。T13 审批点 tab#1 和 tab#2 都显示 approval card。用户在 tab#1 点「批准」后（请求在途 + 网络慢），切到 tab#2 发现还显示「待批准」，又点了一次。两个 POST /input 先后到达。

**可观测信号**：

- 若无 idempotency：`agent_run_events` 里出现两条 `event_type=approval_granted`，`event_idx=47 + event_idx=48`；
- run 状态可能被第二个请求打成奇怪状态；
- 下游 tool 被执行两次。

**根因**：`POST /chat/runs/{run_id}/input` 未设计为幂等。

**建议防线**：

1. 请求体强制要求 `idempotency_key`，推荐格式 `{user_id}:{approval_id}:{decision}`；
2. 服务端实现：接收请求时先查 `agent_run_events WHERE run_id=? AND event_type IN ('approval_granted','approval_denied') AND payload_json->>'$.approval_id' = ?`（注意这违反「不要 JSON 过滤」—— 解决方案：给 approval_id 开一列 index 或专门 `run_approvals` 投影表）；
3. 或者在 run 层面用 `agent_runs.waiting_for`：第二次请求到达时 `waiting_for` 已被清空，服务端直接返回 409 + 明确 `error_code=approval_already_resolved`；
4. 两个 tab 收到同一个 `approval_granted` SSE 事件后都应本地 dedup（F2 的 dedup key 策略复用）。

### F7 — 新 pod 起来把 running run 标 failed

**触发条件**：直接撞当前 `RunStateStore::find_running_runs` 的文档注释：

> `Find runs in RUNNING status (for crash recovery — mark them failed on restart).`

这个语义对「崩溃后无法续跑」的 CLI 场景对，但对 web 场景下的 pod 滚动升级**错**。

**可观测信号**：

- T11 新 pod 起来 3 秒后，`agent_runs.status='failed'`；
- 所有 subscriber 收到 `run_failed` 事件；
- 用户 10:30 回来看到 run 已死。

**根因**：crash recovery 和 graceful shutdown 的区分没在代码层面写死。pod 被 SIGTERM 时应当走 graceful 路径：flush → checkpoint → **保持 running**；真崩溃（OOMKilled）走 fail-fast：标 failed 要求用户 retry。

**建议防线**：

1. 新增 `find_orphaned_running_runs` 和 `find_graceful_paused_runs` 两个方法语义化；
2. shutdown handler 在标记前先写 `checkpoint_json.graceful=true`；启动时按 `graceful` 标志决定 resume 还是 fail；
3. 测试：shutdown 发 SIGTERM，新进程起来后断言 run 继续推进；SIGKILL 场景断言 run 标 failed 且用户可 retry。

---

## 8. 对设计的反馈

本场景是设计文档里 Gap #1 「Run durability」的正面压测。跑完给出以下具体反馈：

### 8.1 设计文档需补强的章节

1. **§"Durable Runs"** 目前列出了 `agent_run_events` 的 schema，但**没有规定 `event_idx` 的生成协议**。需要补充：
   - 是否允许并发 writer？
   - `MAX(event_idx)+1` 的 race 由谁兜底？
   - 建议：新增 `run_counters` 表或明确「单 run 单 writer」的 leader 选举约束。

2. **§"Interaction Semantics"** 的审批描述只有 `approval_requested / granted / denied`，**缺少**：
   - `approval_expired`（durable event + 自动 run 推进）；
   - `approval_retracted`（agent 自己撤回请求）；
   - POST /input 的**幂等协议**（`idempotency_key` 强制、服务端去重实现方式）。

3. **§"Web Client Cache"** 提到 IndexedDB watermark，但**没规定**：
   - watermark 和 event 应用是否必须同一 transaction；
   - 客户端 gap 检测与自愈协议；
   - 同一 session 多 tab 订阅的行为约定（谁是 primary？UI 是否要锁？）。

4. **§"Implementation Plan" Phase 1** 的 Exit criteria 写了「server 重启后 run 可恢复」，但**没区分 graceful shutdown 和 crash**。F7 正好卡在这个模糊地带。建议明确：
   - graceful shutdown → checkpoint.graceful=true → 新进程 resume；
   - crash（无 checkpoint）→ 标 failed，提示用户 retry。

5. **§"Compaction"** 说「保留 L1a 结构化事实」，但**没写** compaction trigger 必须检查活 run。F4 展示这个漏洞。建议加一行：
   > Compaction runs must exclude sessions with an active run (`status IN ('running','waiting')`).

### 8.2 Open Questions

| # | 问题 | 本场景给的线索 |
| --- | --- | --- |
| Q1 | SSE heartbeat 频率和 client 判活阈值由谁定？是每个 runtime config，还是写进协议？ | F5 要求统一：server 15 s 发一次，client 45 s 判死 |
| Q2 | 同一 session 多 tab 订阅是否需要「单一 primary tab」模式（类似 Firestore web SDK）？ | T09 场景表明不需要 primary，但需要事件幂等和 watermark 同步 |
| Q3 | `agent_runs.checkpoint_json` 里应该放什么？Free-form 还是 schema？F1/F7 需要它有 `graceful` 标志和 `last_batch_id`，其他 run 类型需要别的字段 | 建议：定义 `checkpoint_v1` schema，其他字段放 `extra` |
| Q4 | Approval 的 `expires_at` 由谁算？客户端 UI 倒计时需要 server clock；如果 client/server clock skew 怎么办？ | 建议：服务端只发 `expires_at_server`，客户端 UI 用相对时间（`ttl_seconds`） |
| Q5 | Pod 滚动升级时，如何避免「run 分裂给两个 pod」（老 pod 没完全停就新 pod 接管）？ | F1 答案：advisory lock 或单 writer 保证；需要在 §"Durable Runs" 补「run ownership」小节 |

### 8.3 设计能力评估（本场景维度）

| DAUT | 当前设计评级（基于 scenario） | 备注 |
| --- | --- | --- |
| A1 | ◐ | state_revision / transcript_high_watermark / run_event_high_watermark 三元契约清晰，但多 tab 行为未定义 |
| A7 | ⚠️ | schema 到位、契约到位；但 `InMemoryRunStateStore` 仍是默认，且 graceful vs crash 区分缺失。**Phase 1 必须做到可 production 才能声称 ✅** |
| A8 | ◐ | approval 作为 durable event 的 schema 在，但缺 `approval_expired` 生命周期、缺 input idempotency |

---

## 附录 A：完整接口调用序列（紧凑版）

```text
T01 07:05  POST /chat/stream                                           → run_id=r_mig_g1234, SSE stream
T02 07:08  (D01 tunnel 15s, SSE EOF; server continues event_idx 4..9)
T03 07:08  GET  /chat/runs/r_mig_g1234/stream?last_index=3             → replay 4..9, live 10..
T04 07:40  (event_idx=46 approval_requested, agent_runs.status='waiting')
T05 07:41  (D04 WiFi flap 20s, SSE EOF, server state unchanged)
T06 07:41  GET  /chat/runs/r_mig_g1234/stream?last_index=46            → replay 0-row + run_state_sync
           POST /chat/runs/r_mig_g1234/input {approval_id,decision}    → event_idx=47 approval_granted
T07 08:30  (D07 4min10s, server runs to event_idx=98)
T08 08:34  GET  /chat/runs/r_mig_g1234/stream?last_index=46            → replay 47..98
T09 09:10  (Tab#2 opens)
           GET  /sessions/s_mig_2026_05_06/state                       → active_run.run_event_high_watermark=131
           GET  /chat/runs/r_mig_g1234/stream?last_index=0             → replay 0..131 to tab#2
T10 09:20  (Tab#1 suspends)
T11 10:00  (pod restart; graceful shutdown flushes event_idx 187;
           new pod reads checkpoint, resumes; emits event_idx=188 run_resumed_after_restart)
T12 10:30  (laptop wakes)
           GET  /chat/runs/r_mig_g1234                                 → status=running, events_count=205
           GET  /chat/runs/r_mig_g1234/stream?last_index=131           → replay 132..205 (includes 188 resume)
T13 10:45  (event_idx=218 approval_requested)
T14 10:46  POST /chat/runs/r_mig_g1234/input {appr_ddl_0002,approve}   → event_idx=219
T15 11:30  (D16 3s RST, under heartbeat threshold; NO reconnect expected)
T16 11:33  GET  /chat/runs/r_mig_g1234/stream?last_index=~550          → replay ~11 new events
T17 11:35  (event_idx=561 run_finished, agent_runs.status='completed')
```

## 附录 B：本场景不覆盖什么

为避免和其他场景重复，本场景**不压测**：

- LLM context 的 hot/warm/cold 分层（→ S01, S02）
- 巨型 tool 输出与 artifact 化（→ S08）
- 跨设备接力（→ S03；本场景双 tab 是同设备同用户同时间）
- Plan/todo 的反复改动（→ S05）
- 嵌套子 agent 的 run 关系（→ S06, S09, S10）
- 历史检索（→ S11）

本场景的唯一假设压力是：**agent 一直在正确推进一个明确任务，用户和服务端的连接不断碎掉，要求状态不丢、不重、不错。**
