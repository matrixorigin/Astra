# Session Continuity & Crash Recovery

**Status**: Implemented design
**Updated**: 2026-04-16
**Goal**: 让一次真实的 REPL 崩溃/中断之后，用户重启并说一句 `继续` / `resume` / `continue`，系统就能走**真正的恢复路径**，而不是仅仅复用旧的 session id。

## 1. 设计结论

最终方案不再做“普通启动时隐式自动续接”。交互式启动只做**恢复候选探测**与**明确提示**；真正恢复只发生在三种显式意图下：

1. `/resume <session>`
2. `-c/-r` 显式指定恢复 session
3. 启动后还没有活动 session，用户第一句是短 continuation prompt（如 `继续` / `resume` / `continue`）

恢复逻辑统一走一条 shared restore path：恢复 checkpoint / workspace / history / paused plan / durable task / approval overrides，并把 session-memory 中的 **Learnings** 与 **Errors & Corrections** 拼回一条 one-shot `resume_guidance`，在恢复后的第一条 turn 里注入。

## 2. 必须满足的约束

| 约束 | 结论 |
| --- | --- |
| 普通启动不能悄悄续接旧 session | **是**。启动后 `state.session_id` 默认为 `None` |
| 恢复判断必须基于真实 journal 尾部状态 | **是**。使用 shared journal tail classifier |
| 不能依赖“提示词里偷偷注入恢复指令” | **是**。恢复由显式 trigger 驱动 |
| 恢复必须复用已有 checkpoint / `/resume` 基础设施 | **是**。抽出 shared restore helper |
| session-memory 恢复必须按被恢复 session 的 cwd 找文件 | **是**。不依赖当前 cwd，也不依赖 normal combine mode |
| 崩溃后首个 turn 必须覆盖真实 online 场景 | **是**。测试里会模拟 stale server session → 自动重试到新 session |

## 3. 当前代码事实

### 3.1 共享恢复基础设施

- `services/session_journal.rs`
  - append-only JSONL
  - 新增 `classify_session_end_state(session_id)`，对 session 尾部做 bounded reverse scan
  - 区分：
    - `Completed`
    - `Interrupted { kind, resumable }`
    - `Zombie`
- `runtime/pipeline/step_restore.rs`
  - 已能从 heavy checkpoint 恢复 interruption / blocked tools / approval overrides / compaction state
- `astra-turn-core::interruption::build_resume_guidance_with_context(...)`
  - 已能基于 interruption kind + compaction context 生成恢复指导
- `runtime/turn/cloud/memoria_compact.rs`
  - 提供 recovery 专用 `resolve_resume_session_memory_file(session_id, cwd)`
  - 优先 `ASTRA_SESSION_MEMORY_FILE`
  - 否则按 Claude `projects/<sanitized cwd>/<session_id>/session-memory/summary.md` 推导
- `astra-turn-core::cloud_session_memory_extract::extract_learnings_for_backflow(...)`
  - 已能独立提取：
    - `Learnings`
    - `Errors & Corrections`

### 3.2 需要纠正的旧行为

旧实现里有三个问题：

1. `initialize_repl_state()` 会直接把 `last_session_id` 绑定到 `state.session_id`
2. `-c/-r` 只设置 session id，不做真正 restore
3. `/resume` 的恢复逻辑只存在于 slash handler 内部，别的路径无法复用

本设计的核心就是消除这三个分叉。

## 4. 最终架构

### 4.1 Startup: detect, do not resume

`initialize_repl_state()` 现在只做两件事：

1. 保持 `state.session_id = None`
2. 如果最近一次 session 同时满足：
   - end-state 是 `Interrupted(resumable=true)` 或 `Zombie`
   - 且属于当前项目作用域
   则把它记到 `state.pending_recovery`

项目作用域判定规则：

1. 如果当前目录在 git repo 中，并且 workspace 记录了 `git_root`，按 `git_root` 精确匹配
2. 否则按 `cwd` 的 canonical path 做 exact / containment match

**不使用** `mtime < 60s` 之类的跳过规则。
原因：当前没有可靠的 lease / heartbeat / lock 机制，时间窗启发式会误杀“刚崩溃后立即重启”这个最重要的真实场景。

### 4.2 Startup UX

banner 之后只打印提示，不做恢复：

```text
↻ Recoverable session abc12345 detected for this project.
Say continue / resume / 继续 to restore it, or use /resume abc12345.
```

如果用户第一句不是短 continuation，而是一个新的正常请求，则清掉 `pending_recovery`，按新 session 开始。

### 4.3 Explicit triggers

以下三条路径共用同一个 helper：

- `/resume`
- `-c/-r`
- `handle_chat_input()` 中“无活动 session + 有 pending_recovery + 短 continuation prompt”

也就是说：

- `/resume` 不再拥有一套独占恢复逻辑
- `-c/-r` 不再是假恢复
- “继续” 不再只是把几个字发给模型，而是先做真正恢复

### 4.4 Shared restore path

shared restore helper 的职责：

1. 创建 local/cloud restore service
2. 放弃当前 live session 的临时状态
3. 应用 `RestoredSession`
4. 合并 step checkpoint / tool health / approval overrides
5. 恢复 workspace-level state：
   - `session_goal`
   - `goal_progress`
   - `pinned_skills`
   - `discovered_skills`
   - adaptive tuning state
6. 恢复：
   - `history`
   - `last_turn_event`
   - paused plan
   - plan config / corrections
   - durable task contract
7. 重新挂 journal / persist `last_session_id`
8. 把恢复得到的 one-shot guidance 填到 `state.resume_guidance`

### 4.5 Recovered context assembly

恢复后的第一条 turn，会在 normal prompt 前拼接：

1. interruption-derived guidance
   来自 `build_resume_guidance_with_context(...)`

2. session-memory backflow
   从 summary.md 中提取：
   - `Learnings`
   - `Errors & Corrections`

拼接结果写进 `state.resume_guidance`，并保持 one-shot 语义：首个恢复 turn 用掉后即清空。

格式不是隐藏 system injection，而是明确的 recovered context block，例如：

```text
[Recovered session memory]
Use the persisted notes below when continuing the interrupted work.

## Learnings
...

## Errors & Corrections
...
```

### 4.6 No journal-only fallback

旧 `/resume` 在 `restore_session()` 返回 `None` 时，会退化成“只把 journal history 放回上下文，但下一条消息开新 session”。

这个方案已被移除，原因：

- 它不是“恢复”，只是“读旧记录”
- 它会让 `/resume` 的语义一半是真恢复，一半是假恢复
- clean end-state 里，恢复必须要求可用的 workspace/checkpoint state

如果只有 journal、没有可恢复 workspace/checkpoint，就直接报错并引导用户用 `/resume` 查看可恢复条目。

## 5. 首个恢复 turn 的在线行为

最真实的 crash-recovery 场景不是“server 还记得旧 session”，而是：

1. 本地能恢复出旧 session state
2. 但服务端已经丢了那个 session id

因此恢复后的首个 turn 必须允许：

1. 先带着恢复好的 context 向旧 `session_id` 发请求
2. 如果服务端返回 `session not found`
3. 客户端清掉 stale session id
4. 用**同一条 effective message** 自动重试到新 session

这样用户看到的是“恢复成功并继续了”，而不是“恢复到一半又丢了上下文”。

## 6. 触发时序

### 6.1 普通 startup

```text
startup
  -> initialize_repl_state()
     -> pending_recovery = detect(...)
  -> complete_repl_startup()
     -> print banner
     -> print recovery hint (if any)
```

### 6.2 首个 `继续`

```text
handle_chat_input("继续")
  -> session_id is None
  -> pending_recovery exists
  -> restore_session_into_state(...)
  -> consume resume_guidance
  -> build effective message
  -> /chat/turn
```

### 6.3 stale server session

```text
/chat/turn(session_id = old)
  -> 404 session not found
  -> clear stale sid
  -> retry same effective message with new session
```

## 7. 为什么不采用其它方案

### 7.1 不做 implicit auto-resume

因为“最近有个可恢复 session”不等于“用户现在就想恢复它”。
用户可能只是想开始新任务。隐式绑定旧 session id 会让普通 startup 的语义变脏。

### 7.2 不做 startup prompt injection

把 recovery hint 偷偷塞进 system prompt，虽然省代码，但有两个问题：

1. 用户不可见，恢复行为不透明
2. 它不能替代真正的 restore（checkpoint / plan / approval overrides / durable task）

### 7.3 不新增新的 restore payload 字段

`RestoredSession` 现有字段已经足够；额外的恢复语义来自：

- checkpoint interruption
- workspace metadata
- session-memory markdown

没有必要再发明 `interruption_summary` / `error_lessons` 一类字段。

## 8. 测试要求

### 8.1 Unit / focused tests

- journal classifier
  - completed
  - interrupted
  - non-resumable interruption
  - zombie
  - `resume_action` fallback
- recovery session-memory resolver
  - explicit override
  - Claude path from restored cwd
- startup detection
  - same project → `pending_recovery`
  - other project → ignore

### 8.2 Online e2e

必须模拟真实链路：

1. 写真实 journal
2. 写真实 workspace.yaml
3. 写真实 heavy checkpoint
4. 写真实 session-memory summary.md
5. 通过 `initialize_repl_state()` 得到 `pending_recovery`
6. 调用 `handle_chat_input("继续")`
7. mock `/chat/turn` 先返回 stale-session 404，再成功
8. 断言首个恢复 turn 的 request body 包含：
   - interruption guidance
   - recovered `Learnings`
   - recovered `Errors & Corrections`

## 9. 验收标准

以下都满足，设计才算成立：

1. 普通 `astra` 启动不会自动续接旧 session
2. 启动时能提示当前项目存在 recoverable session
3. 第一句 `继续` 会先做真实 restore，再发 turn
4. `/resume`、`-c/-r`、短 continuation 三条路径恢复结果一致
5. session-memory learnings/corrections 会进入首个恢复 turn
6. stale server session 会自动重试到新 session，而不会丢失恢复上下文
7. 不存在 journal-only fake resume fallback

## 10. 实现映射

| 目标 | 位置 |
| --- | --- |
| shared journal classifier | `services/src/session_journal.rs` |
| startup pending detection | `astra-cli/src/cli/repl_runtime.rs` |
| startup explicit restore / hint | `astra-cli/src/cli/repl_startup.rs` |
| shared restore helper | `astra-cli/src/cli/slash_session.rs` |
| short continuation trigger | `astra-cli/src/cli/repl_turn.rs` |
| recovery session-memory resolver | `runtime/src/turn/cloud/memoria_compact.rs` |
| interruption JSON persistence | `runtime/src/turn/agentic_loop_finalization.rs` |
| online e2e | `astra-cli/src/tests/resume_tests.rs` |
