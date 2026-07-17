# Agent Harness 与长任务控制闭环

> 2026-07-17 | 设计目标 + 当前问题 | 面向产品体验与最小演进

## 结论

Astra 当前缺的不是更大的 orchestration 框架，而是一个用户能感知、父 agent 能使用、
Server 和 harness 也能复用的最小控制闭环：

> **启动后可定位，运行中可观察，需要时可干预，干预有确认，结束后结果会送达。**

优先只解决两个高频且高价值的用户旅程：

1. CLI、测试或工具运行很久时，用户和 agent 不会被一个同步调用锁死；能看增量输出，
   必要时输入、纠正或停止。
2. 父 agent 派出子 agent 后可以继续工作；能看到子 agent 的有效进展、补充要求，
   并可靠收到最终结果。

Harness 不另造一套 agent 系统。它应在上述两条链路稳定后，按权限复用同一套观察和控制能力。

## 产品北极星

极致体验不是给用户更多 async、task、run、cursor 选项，而是让用户几乎不用理解它们：

- 快任务自然完成，慢任务自然让出前台。
- 用户不需要记 id、不需要反复问“好了吗”、不需要判断应该 block 还是 poll。
- 后台化后立即回到可输入、可继续工作的状态，而不是进入另一个阻塞页面。
- 系统只在需要注意时打断：需要输入、失败、完成并可继续下一步。
- Agent 自动接回自己发起的工作，但任何新用户指令都拥有更高优先级。
- CLI、TUI、Server 和 harness 面对的是同一个运行事实，不会出现一个入口显示完成、另一个入口仍在等待。

用户最终感受到的应当是：

> **“把耗时工作交给 Astra 后，我可以继续表达意图；它知道工作在哪里、什么时候该回来，
> 也不会用无意义的状态查询打扰我。”**

衡量一项设计是否值得做，优先看它能否消除以下真实摩擦：

| 用户摩擦 | 理想结果 | 产品响应 |
|---|---|---|
| 不知道是不是卡住 | 始终知道在运行、等待输入还是失败 | 稳定状态 + 最近有效输出 |
| 为了等结果反复询问 | 完成后系统自己回来 | 事件驱动 notification |
| 后台化后 agent 也停了 | 只释放阻塞点，不停止协作 | Tool 返回后继续模型决策 |
| 多个子任务消息刷屏 | 只看到需要决策的变化 | 生命周期自动、语义 checkpoint、批量合并 |
| 旧任务结果打断新要求 | 最新用户意图永远优先 | 合并上下文，不抢占用户 turn |
| 不同入口行为不同 | 在任何入口都能观察和控制 | Canonical runtime control path |

## 1. 第一性原则

### 1.1 工作一旦开始，就是可寻址的运行实体

一个进程、长工具调用或子 agent，生命周期都可能长于发起它的单次 tool call。
只返回“正在运行”或在 UI 中显示一行状态不够；调用方必须持有稳定身份，之后才能继续观察和控制。

稳定身份不等于现在就统一所有对象。第一阶段继续使用已有的 `task_id` 和 `run_id`，
先统一行为语义，避免为了抽象整齐而迁移全部存储和协议。

### 1.2 闭环不是“有进度条”，而是 observe → decide → act → acknowledge

- **Observe**：知道目标是谁、当前状态、最新有效输出以及是否仍有进展。
- **Decide**：用户、父 agent 或 harness 能基于新信息判断是否继续。
- **Act**：向正确目标补充语义指导、写入进程 stdin，或执行暂停/取消。
- **Acknowledge**：系统明确反馈操作已排队、已生效或被拒绝，而不是让调用者猜。

如果缺少目标身份、干预能力或确认，系统仍然是开放环路。

### 1.3 三种输入必须分开

它们面向不同接收者，不能都叫 message：

- **进程输入**：写入 PTY/stdin，立即影响正在运行的 CLI。
- **Agent guidance**：进入指定 agent 的 mailbox，在下一个模型边界成为新上下文。
- **运行控制**：暂停、恢复、转后台、取消，改变调度或生命周期。

明确区分后，用户才能理解“为什么这条话不会立刻改变正在执行的 bash”，实现也不必靠隐式约定猜意图。

### 1.4 事实由 runtime 报告，语义由 agent 报告

Runtime 应自动、可靠地报告 `started`、`waiting`、`completed`、`failed`、`cancelled` 等事实。
子 agent 只在被阻塞、需要决策、完成重要里程碑或发现会改变父任务方向的信息时，发送语义 checkpoint。

要求模型频繁“主动汇报”既浪费 token，也不可靠；完全依赖父 agent 轮询又会增加延迟和调用成本。
合理组合是：**生命周期自动推送，语义进展按事件汇报，详细过程按需拉取。**

### 1.5 前台/后台是等待策略，不是两种执行

一个命令从前台转到后台时，不应重启、丢输出或更换身份。短任务保持同步最省心；
只有超过合理等待时间，或调用者明确要求时，才把同一个运行实体交还为可继续观察的句柄。

### 1.6 每一层复杂度都必须对应可验证的用户价值

新增设计在进入实现前应回答：

1. 它具体减少了哪一种等待、失控或误路由？
2. 能否在现有 run、task、mailbox、progress callback 上补齐，而不是新建平行系统？
3. CLI、Server、TUI、父 agent 中至少哪两个调用方会真实复用？
4. 如果删除这层抽象，用户体验会在哪个场景退化？

答不出来就暂不建设。

### 1.7 用户意图优先于所有后台事件

后台完成、子 agent 汇报和 harness 建议都是上下文，不是比用户更高优先级的新命令。
同一时刻出现多个输入时，默认顺序是：

1. 最新用户 steering / cancel；
2. 需要用户或父 agent 决策的 blocked / needs-input；
3. terminal result；
4. 普通 progress / advisory。

通知到达时如果用户已经输入，不抢先启动一个独立模型回合；把通知与用户输入合并，在同一个边界处理。
如果新要求使旧任务不再相关，agent 应先确认是否取消或忽略旧结果，不能让迟到的 completion 把方向拉回去。

## 2. 目标体验

### 2.1 长时间 CLI / 测试 / 工具

目标流程：

1. 短命令仍像现在一样直接返回，不增加概念和操作。
2. 命令超过前台等待阈值后，同一进程自动 yield；返回 `task_id`、状态和最近输出，
   TUI 不丢失已经显示的内容。
3. 用户、agent 或 Server 可以按 cursor 读取新增输出，而不是重复拉取全部日志。
4. 交互式进程显式声明 stdin 能力后，可以继续写入输入；非交互式任务不展示无效入口。
5. 调用者可以取消；最终退出码和尾部输出会可靠送达，不要求持续轮询才能知道结束。
6. 用户此时输入给 agent 的话会明确显示为“已排队，当前工具结束或 yield 后生效”；
   写给进程的内容则走单独的 stdin 操作。

这里最重要的不是“所有工具异步化”，而是长任务不会把整个决策循环锁死。

用户不应被迫选择同步或异步。默认策略是：先前台执行，超过交互等待预算后原地 yield；
只有模型已经明确知道结果暂时不需要时，才一开始就后台运行。阈值是运行时策略，不进入普通用户心智。

### 2.2 父 agent 与子 agent

目标流程：

1. `spawn` 在子 run 被接受并开始后返回 `run_id`，不等待子 agent 完成。
2. 父 agent 可以继续分析、执行其他独立工作，或显式 `wait`；等待应能被新消息、用户 steering
   和子 agent 状态变化唤醒。
3. 父 agent、TUI、Server 都以同一个 `run_id` 查看进度和 transcript，并向精确子节点发送 guidance。
4. Guidance 返回 `queued`，实际注入后产生 `applied`；目标已结束时返回明确拒绝或转为 follow-up，
   不静默丢失。
5. 子 agent 知道自己的 `run_id`、`parent_run_id` 和任务边界。被阻塞或出现方向性发现时主动汇报，
   普通过程不刷屏。
6. 子 agent 完成时，结果自动进入父节点 mailbox；父 agent 即使没有正在 `wait` 也不会漏掉。

这使 delegation 从“阻塞式远程函数调用”变为真正可协作、可纠偏的并发工作。

普通用户不需要复制 child id。TUI 以任务名称展示父子树，选中节点即可查看、指导或停止；
精确 id 留给协议、日志和高级操作。成功的普通 checkpoint 折叠，blocked、冲突和失败自动展开。

### 2.3 Harness

Harness 的目标不是默认接管 agent，而是成为受控的第三类调用方：

- 能按 `run_id` 识别 root、父子关系和每个节点的状态，而不是把子节点快照混在父 sink 中。
- 默认只观察；显式授权后，复用相同的 `guide`、`pause`、`cancel` 操作。
- 所有干预记录发起者、目标、原因和结果，便于 UI 展示和事后审计。
- Harness 需要语义判断时可以 harness 一个 agent；agent 也可以被上层 harness 管理，
  但二者都仍是普通 run，不引入特殊的递归执行模型。

只有当长任务和父子 agent 的控制面稳定后，这一层才有闭环价值。现在先补 identity，
不先建设自治 supervisor、工作流 DSL 或复杂策略引擎。

### 2.4 TUI 的默认体验

长任务从前台转后台时，不应该自动把用户困在任务详情页。理想交互是：

1. 下一帧恢复 composer 焦点，并显示一条短确认：`Tests running in background · 18s`。
2. 任务进入常驻但不抢眼的 activity strip：名称、状态、elapsed、最近一条有效输出。
3. 用户展开时才看完整增量输出和高级操作；默认不展示内部 id、offset、output path。
4. 可以直接执行 View、Stop；只有进程支持时才显示 Send input，避免无效按钮。
5. 成功时折叠为一行摘要；失败、等待输入或被 harness 阻断时自动展开最小必要证据。
6. Agent 在后台继续工作时，composer 仍可输入；新输入在下一个模型边界优先生效，并明确显示 queued/applied。

UI 不把每个 progress delta 写进聊天历史。聊天只保留有长期语义的事件，实时字节流属于可展开的运行详情。

### 2.5 后台事件如何影响 LLM

| 事件 | TUI 行为 | LLM 行为 |
|---|---|---|
| 转入后台 | 短确认，恢复 composer，固定 activity row | Bash tool 返回后继续一次正常决策 |
| 普通 output delta | 更新详情和最近输出 | 不自动进入上下文，不触发模型调用 |
| needs-input / blocked | 提升显著性并说明目标 | 下一个模型边界注入；空闲时主动恢复 |
| completed / failed | 成功折叠、失败展开证据 | 注入或自动触发 notification turn |
| no-recent-output | 仅弱提示“仍在运行，近期无输出” | 不自动调用模型 |
| 新用户输入 | 立即显示 queued/applied 状态 | 优先于所有未消费后台事件 |

这个分层保证 UI 足够实时，模型上下文和调用成本却不会随 stdout 线性增长。

## 3. 最小公共控制语义

第一阶段不要求一个新的万能 `Execution` 类型，只要求已有实体遵守一致的体验约定：

| 目标 | 身份 | 观察 | 输入 | 生命周期控制 | 完成通知 |
|---|---|---|---|---|---|
| Agent run | `run_id` | 状态、进展、transcript | `guide` | pause / resume / cancel | result → parent mailbox |
| Process / background task | `task_id` | 状态、cursor 后的输出 | `write_stdin`（有能力时） | cancel | exit code + tail output |

最小事件只有：

- `started`
- `progress` / `output_delta`
- `guidance_queued` / `guidance_applied`
- `completed` / `failed` / `cancelled`

Cursor 先复用当前 output offset / sequence 能力；是否跨进程持久化服从原实体已有的 durability，
此阶段不新建通用 event store。只有 agent 与 process 的实现持续出现相同代码和一致生命周期后，
再评估是否值得抽取统一 `ExecutionHandle`。

### 3.1 高价值能力边界

| 能力 | 用户价值 | 可复用现有基础 | 决策 |
|---|---|---|---|
| Ctrl+B 后继续 agentic turn | 极高：后台化真正释放阻塞 | Detach marker、Waiting 分支 | 立即做 |
| Terminal event 自动恢复模型 | 极高：消除手动追问 | `pending_bg_notifications`、event loop | 立即做 |
| Agent 消息精确路由和确认 | 极高：纠偏不能静默失败 | Mailbox、run intent | 立即做 |
| Spawn 与 wait 分离 | 极高：父 agent 获得真实并发 | Durable child run、`get_result` | 紧随其后 |
| Output cursor 的 CLI/Server parity | 高：远程和本地体验一致 | `task_output` offset、progress callback | 做 |
| PTY `write_stdin` | 高但场景较窄：解决交互式 CLI | Detach 后 stream ownership | 第二批 |
| Harness 自治 supervisor | 价值尚未验证、风险高 | 仅有 snapshot/verifier | 暂不做 |
| 通用 `Execution` / workflow DAG | 暂无直接用户价值、迁移成本高 | 无必要 | 不做 |

第一批功能应尽量是对现有链路的“最后一公里”修复，而不是新建框架。

## 4. 实施前基线问题

本节保留本轮重构前的真实行为，用于解释设计取舍；已落地修复以第 10 节为准。

### 4.1 长任务：TUI 能看到活动，但 agent 的控制循环仍被同步调用卡住

当前 Bash 前台执行会持续向 TUI 报告行数和字节数，用户也可以用 Ctrl+B 把运行中的命令转到后台。
这已经解决了一部分“界面像死掉”的问题，但没有形成跨调用方闭环：

- `bash` 的公开参数只有 command、timeout、force；没有 `yield_after`、后台句柄或 stdin 能力声明。
- 前台 progress 主要是行数/字节数；模型没有可消费的增量输出内容，无法根据新日志调整判断。
- 后台 `task_output` 已有 offset、阻塞/非阻塞读取和超时，但普通 Server tool surface 刻意不暴露
  `task_output`、`task_stop`、`task_list`，CLI 与 Server 的能力不对称。
- Tool progress callback 类型支持 output delta，但生产执行路径目前只稳定发 started/completed，
  没有把工具真实增量输出接到 Server/WebSocket 消费者。
- 非 Bash 的长工具仍是同步调用；没有通用的 observe / cancel 句柄。
- Root guidance 虽可在运行中排队，但 agentic loop 只能在当前 tool call 返回后读取。
  UI 已提示这一点，系统却没有自动 yield 让 agent 尽快重新获得决策权。
- Ctrl+B 成功后 TUI 会主动打开该任务详情。这对检查输出有用，但对只是想释放前台、继续输入的用户
  是一次额外上下文切换；更合理的默认是恢复 composer，并把详情作为可展开入口。

结果是：**用户可能看得到进度，但父 agent 既读不到有效增量，也不能在当前调用内做新的决策。**

#### Ctrl+B 后当前实际发生什么

以一个很长的 `cargo test` 为例，当前 CLI 的真实链路是：

1. 命令默认一直在前台运行；Astra 不会按时间自动转后台，模型也没有 `run_in_background` 参数。
2. 用户按 Ctrl+B 后，runner 停止前台读取，把**同一个**子进程及 stdout/stderr 所有权交给
   `BackgroundTaskRegistry`。不会 kill + respawn，也不会丢掉之前的输出。
3. Bash tool 只等待 TUI 完成 adoption acknowledgement；正常是进程内的短握手，硬上限 30 秒，
   **不是等待测试完成**。
4. Tool result 返回 `<bash_detached>` 和具体 `task_id`。Runtime 看到结构化 detach marker 后，
   立即把本次 agentic turn 结束为 `Waiting(background_task_detached:<id>)`。
5. Runtime 有专门保护，不允许同一 turn 再启动一轮 LLM 去自动调用 `task_output`。
   当前提示甚至要求模型“end your turn now and let the user drive next steps”。
6. TUI 打开该后台任务的详情，继续采集和展示输出。任务结束后，TUI 显示 completed / failed / killed，
   并把结构化 notification 放入 `pending_bg_notifications`。
7. 这条 notification 只在**下一次用户输入触发的新 turn** 中注入模型上下文；当前不会因为测试结束
   自动唤醒 root agent。

所以当前答案是：**压入后台后很快返回 UI，但不是返回给 LLM 让它继续安排工作，而是直接结束当前
模型回合。** LLM 不会在后台自动做其他独立事项，也不会自动 poll。用户下一次要求看进度时，
模型可以调用一次 `task_output(block=false)`；用户明确要求等待时，可以调用
`task_output(block=true)`，它会在有新输出、任务结束或超时后返回。

这个行为的优点是可预测、不会发生无意义轮询，且 foreground → background 的进程移交很扎实。
缺点也很直接：Ctrl+B 在产品语义上变成了“后台化并停止 agent”，而不只是“释放这个阻塞点”；
后台任务完成后，闭环仍依赖用户再次说话。

#### Claude Code 当前行为

本地 `~/claudecode` 采用另一种调度语义：

- 模型可以在调用 Bash 时设置 `run_in_background=true`；用户也可以 Ctrl+B 原地后台化已注册的前台任务。
- 在启用 KAIROS assistant mode 时，main agent 的阻塞命令超过 15 秒还会自动后台化；普通模式下
  是否自动后台化仍受 feature、命令类型和 timeout 约束，不能理解为所有安装都固定 15 秒。
- 后台化后 Bash tool 立即返回 task id 和 output path，随后模型循环可以继续：做不依赖测试结果的工作、
  简短告知用户，或结束当前回复。
- Prompt 明确要求：会收到完成通知，不要 sleep、不要主动 poll。
- 任务结束会 enqueue `<task-notification>`。主线程空闲时 queue processor 会把它作为独立输入再次调用模型；
  如果模型仍在运行，合适优先级的 notification 也可在后续模型边界作为 attachment 注入。
- `TaskOutput(block=true|false)` 仍存在，但已被标为 deprecated，正常完成路径更鼓励等 notification 后读取
  output file，而不是持续检查状态。

因此 Claude Code 的默认体验更接近：**后台化释放 tool call → agent 继续协调 → 完成事件主动让 agent
重新获得决策权。** 它不是在前台隐式等结果，也不鼓励轮询。

#### Astra 应采用的最小目标语义

不需要复制 Claude Code 的任务框架，只需调整两个边界：

1. Backgrounding 应结束当前 Bash tool call，但**不应强制结束整个 agentic turn**。把 `task_id` 和最近输出
   交给模型，允许一次正常的后续决策；模型可以做独立工作，也可以告知用户后结束。
2. 对刚后台化的同一 task 保留 anti-poll guard：没有用户明确要求时，拒绝紧接着调用 `task_output`，
   但不阻止其他工具和 reasoning。
3. Terminal / needs-input notification 应进入正在运行 agent 的下一个模型边界；root 空闲时自动形成一个
   notification turn。仅状态计数和 no-recent-output advisory 不自动触发昂贵模型调用。
4. 多个短时间内完成的后台任务合并成一次 notification turn；若用户输入已经排队，与该输入一起送达，
   避免抢占用户和制造额外回合。
5. Bash 保持前台默认，同时提供模型显式 background 选择，并为明显超出交互等待预算的命令增加自动 yield。
   三种入口都复用同一个进程移交和 notification 路径。

这比“后台后自动 poll”更省调用，也比当前“后台后停止一切，等用户再说话”更完整。

### 4.2 子 agent：人可以从 TUI 旁路管理，父 agent 本身却仍是阻塞和不一致的

对“开 subagent 后父节点是不是不能再输入”的准确回答是：

- 当前公开 `agent(spawn)` 语义明确是 foreground / blocking；父模型停在这次 tool call 内，
  子 agent 结束前不能继续产生下一次模型调用或工具调用。
- 人仍可操作 TUI。TUI 已能查看指定子 agent transcript，并直接 guide、pause、resume、stop。
- Server 也有按精确 run 写入 run intent 的入口。
- 但这些是 UI/控制面旁路，不代表父 agent 已拥有同样能力。

父子协作目前还有四个断点：

1. **Server 的模型侧 `agent.send_message` 不可用。** 该 action 被路由到固定 runtime binding error；
   CLI 有另一条可选实现，造成同名工具跨执行面行为不同。
2. **存在两套消息契约。** Consolidated `agent.send_message` 以 agent id 为 recipient，
   另一套 standalone messaging 支持 `parent`、broadcast 和 progress/question/result 类型，
   但它不是当前 canonical public surface。Schema、runtime interception 和 handler 已经漂移。
3. **父子身份在 runtime 中存在，但不是稳定的模型契约。** Spawn state / server subrun context 保存了
   parent id 和 mailbox，子 agent 的系统提示却只描述“完成专门任务”，没有 parent identity、
   checkpoint 条件和结果送达责任。
4. **Guidance 只在模型边界生效。** 如果子 agent 正卡在长工具里，TUI/Server 发来的纠正也要等工具返回；
   当前缺少明确的 queued/applied 确认。

因此现在是“操作员可以管理子节点”，还不是“父 agent 与子 agent 天然形成反馈闭环”。

### 4.3 Harness：目前是观察/阻断 hook，不是可纠偏的 supervisor

Runtime harness 已有 hook、snapshot、history、SSE 和 verifier，这是有价值的基础；问题在于它无法
可靠回答“我在观察哪个子节点，以及如何把判断反馈给它”：

- Snapshot identity 主要是 session / turn / model，缺少稳定的 `run_id`、`agent_id`、`parent_run_id`。
- 子 agent 的 observe-only harness 可以写入父 sink，但记录无法可靠关联到运行树中的节点。
- Verdict 主要是 Continue / Block / Pause；adjust commands 是预算、断点、成本、watch tool，
  没有对指定 agent 的 inspect / guide / cancel。
- Snapshot 反映 hook 时刻的状态，却不承载长工具的可消费输出流；harness 看见“工具还在跑”，
  不一定看见足够信息去做修正。

所以 harness 目前可以看、可以挡，但不能完成“观察 → 判断 → 修正 → 确认”的内部闭环。

### 4.4 CLI、TUI、Server 与模型侧能力不是同一个产品契约

当前能力散落在 TUI keybinding、local run control、server run intents、background task tools、
agent tool 和 standalone messaging 中。每一块单独看都有合理实现，但用户会遇到：

- TUI 能做，父 agent 不能做；
- CLI 能做，Server 的同名工具不能做；
- 状态能看到，输出内容看不到；
- 消息已发送，但不知道何时生效；
- 子节点结束了，却依赖调用方继续 poll 才知道结果。

根因不是缺少更多工具，而是没有一条 canonical control path 被所有入口复用。

## 5. 最小演进顺序

### P0：闭合现有 Ctrl+B 路径

这一阶段不增加新任务类型，只改变已经存在的 detach 后行为：

1. Detach 后结束 Bash tool call，但不再无条件把整个 agentic turn 返回为 Waiting；把 `task_id`、状态和
   有界输出 tail 交给下一次模型决策。
2. 对刚后台化的同一 task 启用 anti-poll，阻止立即 `task_output`；其他独立工具正常执行。
3. TUI 默认恢复 composer 并固定 activity row，不自动打开详情；用户主动 View 时再展开。
4. completed / failed / needs-input 进入正在运行的下一个模型边界；root 空闲时触发合并 notification turn。
5. Notification 与已排队用户输入合并，用户 steering 先处理；普通 progress 和 quiet advisory 不触发模型。

完成这一阶段，现有 Ctrl+B 才从“终止等待”升级为“释放阻塞并最终自动接回结果”。

### P0：修复 agent guidance 的正确性

1. 选定一套 canonical agent guidance 契约，让 CLI 和 Server 的 `agent.send_message` 行为一致；
   支持精确 child、parent 和必要的 broadcast。
2. 发送返回 queued，注入返回 applied，目标已结束返回 rejected / follow-up；不允许成功响应后静默丢失。
3. 将 standalone messaging 降为内部适配层或迁移后删除，避免继续维护两个公开真相。
4. TUI 对指定子节点的 direct guidance 复用这条路径。

完成这一阶段，人工纠偏、父 agent 协作和未来 harness 才建立在可信路由上。

### P1：让 Bash 自动管理等待预算

1. 保持前台默认；增加模型显式 background，并在超过交互等待预算后自动 yield。
   Ctrl+B 是用户主动提前 yield，三条入口复用同一个进程移交，`task_id` 不变。
2. 把真实 output delta 接入 cursor 读取和 Server/WebSocket；模型按需观察，避免把所有 stdout
   自动灌入上下文。
3. 所有后台 Bash 支持 status 和 cancel；只对 PTY/交互式任务提供 `write_stdin`。
4. CLI 和 Server 使用相同语义，TUI 只呈现 canonical state，不持有独立事实。

先只做好 Bash，因为它覆盖测试、编译、服务启动和大多数 CLI harness 场景。
其他工具只有出现真实长等待需求后，再接入相同语义。

### P1：让父子 agent 真正并发

1. `spawn` 在 accepted/running 后返回 child `run_id`；完成不再是 spawn 的返回条件。
2. 复用或收敛 `get_result` 为显式 observe / wait；wait 可被用户 steering、mailbox 或状态变化唤醒。
3. Runtime 自动把 started、blocked/needs-input、completed/failed 送给父节点；最终结果不依赖轮询。
4. 子 agent prompt 只增加最小协议：知道 parent、何时 checkpoint、最终向父节点交付什么。
5. 父 agent 收到 checkpoint 后可以立即 guide；同一子节点已结束时，消息显式转为 follow-up 或拒绝。

用户价值：父 agent 真正获得并发能力，同时保留当前 TUI 对子节点的可见性和人工接管能力。

### P2：Harness 只做控制面的复用者

1. 在 harness snapshot / event 中补 `run_id`、`parent_run_id` 和节点角色。
2. Harness 按 run tree 订阅节点状态和增量事件。
3. 默认 observe-only；在明确 policy 下调用同一个 guide / pause / cancel，并留下审计记录。
4. 只有真实场景证明需要自动判断时，才让 harness 调用专用 agent；不先引入通用自治 supervisor。

## 6. 明确不做

- 不建设通用 DAG / workflow DSL。
- 不让所有短工具都异步化。
- 不为了 API 外观统一，立即把 agent run、process task、durable task 全部迁移成一个新类型。
- 不要求模型上报每一步，也不把每个 stdout chunk 自动塞进模型上下文。
- 不要求普通用户设置等待阈值、选择 block/non-block 或理解 cursor；这些是内部策略和高级操作。
- 不让 LLM 用循环 `task_output` 模拟事件订阅；状态未变化时不产生模型调用。
- 不把 TUI 当作第二个 runtime；TUI 展示和控制 canonical state。
- 不默认允许 harness 自主修改所有子节点；控制能力必须有 scope 和审计。
- 不先做分布式 scheduler、跨节点任意迁移或完整 event-sourcing 重写。
- 不照搬 Codex / Claude Code 的工具命名和内部结构，只借鉴已经证明有用户价值的交互模式。

## 7. 验收场景

这些场景比“新增了多少 trait / endpoint”更能说明设计是否成立：

1. 小于等待预算的普通命令行为不变，不出现 task id、后台提示或额外模型回合。
2. 一个持续数分钟的测试命令超过预算或被 Ctrl+B 后，不重启、不丢输出地返回 `task_id`；
   composer 恢复，任务进入 activity strip，agent 可以立即做不依赖测试结果的工作。
3. 任务运行期间，TUI 实时更新输出；在状态没有语义变化时，LLM 不调用 `task_output`，也不产生模型回合。
4. 测试完成或失败后，即使用户没有再次输入，root agent 也会收到一次合并后的 notification turn，
   读取必要证据并继续原目标，而不是只说“任务完成”。
5. Completion 到达前用户输入新要求时，两者在同一模型边界处理；用户要求优先，旧任务结果不把方向拉回去。
6. 命令等待交互输入时，UI 明确显示 needs-input；只有支持 PTY 的任务才提供 Send input，输入写入正确进程。
7. 命令运行时用户给 agent guidance，界面显示 queued；命令 yield 后显示 applied，agent 基于新要求调整下一步。
8. 父 agent spawn 子 agent 后立即继续独立工作；中途能读 checkpoint、补充要求；子节点结束后结果自动送达。
9. TUI 与 Server 对同一个 child `run_id` 执行 guide，得到一致的 queued / applied / rejected 语义。
10. Harness 能显示完整 run tree，记录每次干预的来源、目标和结果；没有身份不明的混合 snapshot。

### 7.1 产品级成功标准

- 用户等待长任务时需要手动查询状态的次数：**0**。
- 状态未变化时由系统产生的 LLM poll：**0**。
- 前台转后台造成的进程重启、输出缺口或重复执行：**0**。
- Terminal / needs-input 事件被模型消费：**效果上一次**；以消息 id / 状态转换去重，失败回合可安全重放，
  不以“提前出队”冒充成功消费。跨进程恢复服从所在 transport 的 durability。
- 新用户 steering 被迟到后台事件覆盖：**0**。
- UI 默认暴露的内部实现概念：只显示任务名称和状态；id、cursor、output path 按需展开。

## 8. 当前实现锚点

- Bash / background task schema：`crates/astra-tools/src/schemas.rs`
- CLI Bash progress 与 background task handler：`crates/astra-cli/src/cli/stream/stream_render.rs`、
  `crates/astra-cli/src/edge_tools.rs`
- 前台转后台：`crates/astra-tools/src/detach.rs`
- Root guidance queue：`crates/astra-cli/src/cli/turn/local_run_control.rs`、
  `crates/runtime/src/turn/agentic_loop/execution_phase.rs`
- 子 agent TUI 控制：`crates/astra-cli/src/tui/bottom_pane/in_flight_agents_view.rs`
- Server exact-run intents：`crates/runtime/src/server/run/handlers.rs`
- Agent messaging 路由：`crates/runtime/src/server/tool_agent_runtime.rs`、
  `crates/runtime/src/orchestration/agent_tool.rs`、`crates/astra-messaging/src/send_tool.rs`
- Parent / mailbox state：`crates/runtime/src/orchestration/spawner.rs`
- Harness snapshot 与 adapter：`crates/astra-harness/src/lib.rs`、
  `crates/runtime/src/turn/harness_adapter.rs`

## 9. 外部实现只提供的设计验证

本地 Codex 和 Claude Code 实现验证了几个值得采用的交互事实：

- 长进程要在首次 yield 前先注册，之后才能用稳定 id 继续 poll / write stdin。
- 输出 delta 需要 cursor，避免重复日志；等待和读输出可以异步，但短命令仍保持直接返回。
- Spawn、wait、message/follow-up、interrupt 是不同意图，混成一次阻塞式调用会损失并发和纠偏能力。
- Process stdin、agent message、lifecycle control 必须是不同通道。

这些是体验原则，不是要求 Astra 复制其类型、工具数量或调度架构。

## 10. 2026-07-17 实施状态

本轮按“先闭合现有链路，不新建万能执行框架”的原则完成了第一批落地：

- Ctrl+B 只结束当前 Bash tool call，不再强制结束整个 agentic turn；同一 turn 内对刚后台化任务的
  `task_output` 由 runtime 硬性延迟，其他独立工作不受影响。
- TUI 后台化后保留 composer，不再自动打开任务详情；任务身份、输出和进程所有权仍沿用原
  `BackgroundTaskRegistry`，没有重启进程或新建第二套事实源。
- Shell terminal 事件和 child terminal / waiting 事件会进入 active run 的下一模型边界；root 空闲时，
  TUI 先做短窗口合并，再发起无可见伪用户消息的 notification turn。普通 output delta 和
  no-recent-output 只更新 UI，不触发 LLM。
- Runtime notification 使用独立 required-context lane，不写成 user history，也不覆盖 latest user goal；
  空闲唤醒只用非空 runtime envelope 满足 provider message 约束，settlement 仍以空 logical user line
  提交，因此 envelope 不会伪装成用户历史。Notification 与用户 steering 同时到达时，payload 明确以
  用户意图为最高优先级；内部 payload 还带进程 nonce，Server 用户输入不能伪造 runtime lane。
- CLI 与 Server 的模型侧 `agent.send_message` 已收敛到 shared runtime handler，支持精确 child / peer、
  delegation boundary 内的 exact `run_id`、`parent` 和 broadcast；只有 mailbox context 的 skill sub-run
  也复用同一路由核心。发送结果为 `queued`，接收模型边界产生 `applied` evidence，已结束、不存在或
  跨 delegation boundary 的 direct target 返回 `rejected`。CLI 原先 500 余行的第二套解析、schema、
  payload 和发送实现已删除，只保留 mailbox binding context。
- 公开 `agent.spawn` 改为 identity-ready 后返回，不再等待 child terminal result；child prompt 包含自己的
  `run_id`、`agent_id`、parent identity 和最小 checkpoint 规则。终态结果自动送入 parent mailbox，
  `get_result` 保留为显式 observe / wait。
- Parent mailbox 暂时离线时，router 只对 parent-target 消息做有界延迟投递，并在对应的稳定 mailbox
  再次注册时重放；发送失败与 parent 重新注册之间的竞态由 registration gate 关闭。别名保存完整
  `AgentAddress`，避免 DB transport 只拿到 run id 后把消息持久化给伪造 agent id。Direct guidance 不进入
  该队列，避免把已终止 child 误判为“稍后可达”。队列满时拒绝新消息并让发送方看到失败，不再静默淘汰
  此前已经确认接收的消息；过期消息不会在重连后复活。
- Runtime notification 采用两阶段消费：到达模型边界只产生 applied evidence，只有整个回合成功 settlement
  才真正提交；模型失败、中断或认证/session retry 时，事实会回收到下一边界，不会因为提前 ack 丢失。
- Interactive CLI 的 turn run 与 session-stable root mailbox 已显式关联；child 对 parent 的 checkpoint、终态，
  以及 parent guidance 的 applied ack 都回到真实存活地址。Server root 同样按 session 注册稳定 mailbox，
  turn 结束时把最后一批未消费消息有界停放，下一 turn 重放。
- Broadcast 固定表示“当前 delegation 的 peers”：root 对直接 children，child 对 siblings；不会因为某个
  nested child 临时又有 children 就改变 `*` 的含义。发送者的 transport self-echo 被消费但不注入模型。
- Mailbox transport 保留完整 payload；模型边界使用包含 request / response 正文的有界预览，避免 64 条消息
  把上下文无上限撑大。Terminal 预览被截断时，模型可按 agent id 用 `get_result` 读取完整结果。
- 一次性 CLI、headless task 和 App Server 会在父模型结束后保留 child 的进度/审批通道，做最长 30 秒的
  有界收口，并把 completed、failed、waiting、cancelled、deadline-exceeded 统一呈现给文本和结构化调用方。
  Deadline 不再裸 `abort` 留下僵尸 agent，而是走正常非用户驱动的取消、归档和终态通知路径。

### 10.1 本轮静态复审后确认的产品边界

- Interactive TUI 已能在 root 空闲时由 terminal / needs-input 事件触发合并 notification turn；普通进度
  不唤醒模型，用户 composer 非空时不抢占。
- 一次性表面没有“下一 turn”，因此选择等待一个有界窗口并直接聚合 child 结果；这与 interactive
  表面的“立即释放父节点、事件驱动回来”是有意的体验差异，不要求用户理解调度细节。
- Durable Server 已消除 root 无 mailbox 导致的消息丢失，并能在下一 session turn 重放离线消息；当前尚未
  在 root 已终止且没有新用户输入时自动创建一个新的 Server LLM turn。该能力需要 durable wake ownership、
  计费和用户意图优先级共同落地，不能用后台 `tokio::spawn` 伪造。
- `message_type=result` 只表示模型的语义报告，不再伪造 runtime `Completed`；真实终态始终由 runtime 自动发送。

### 10.2 Session `5ab1d01f-b933-4258-ba85-8536d9b35bd4` 复盘

这次真实使用暴露出“局部闭环已经存在，但闭环之间会重复、误判”的问题。该 session 只有 4 条顶层
turn 记录，却产生了 119 个 LLM round；其中 root agent 占 94 round、约 114.7 万输入 token，并调用
`task_board` 52 次。两个修复回合分别持续约 12 分钟和 36 分钟。高消耗不是任务本身必需，而是以下
反馈失真级联造成的：

1. `agent_fanout.start` 实际返回 3 个 `launched` child 和稳定 `group_id`，顶层状态却是 `started`；通用
   tool-result classifier 不认识 `started`，把成功回执记为 `ok=false` 和 unclassified tool error。
2. Parent 随后用 `get_results` 收齐了 3 个结果并输出汇总，但 child attention hint 在结果收集前一瞬间
   已进入 local run-control 队列。结算时系统没有用 `result_collected` 再确认，因而 200ms 后创建了一个
   空 logical-user notification turn，重复汇报同一结果并再次询问是否修复。
3. 三个 reviewer 的 finding 被直接提升为 P0 / Critical，没有先核对当前锁、容量和投递不变量。后续 root
   先把 router race、队列 eviction、promotion rollback 判为 false positive 并取消 task-1/2/3，又创建
   等价的 task-14/15/16、修改代码并标记 completed，最后再次撤销。Task Board 一度把“不存在于最终代码的
   修复”显示为 completed，说明任务状态和代码事实没有完成闭环。
4. `Ctrl+T` handler 的注释声明 Ctrl+B 是后台任务入口，代码却在普通 Task Board 路径优先调用
   `open_background_task_view`。旧测试使用空 registry，错误分支恰好返回 false，因此形成假绿；真实 session
   有后台任务时就稳定打开错误面板。
5. Runtime notification 的顶层 `turn.user_input` 虽为空，但 provider 使用的非空 internal envelope 仍以
   `role=user` 出现在 durable transcript。它没有冒充用户的 turn record，却仍污染了逐条 transcript 语义；
   后续需要 typed internal input / persistence filter，而不是依靠固定自然语言识别。
6. 三个 reviewer 实际分别运行 10 / 5 / 10 个 round、产生 32 / 11 / 27 次 tool call，但
   `agent_terminated.turns_completed` 全部为 0。CLI executor 已持有准确的 session turn number，返回给 spawner
   的结果契约却遗漏该字段，导致终态 journal 和历史面板丢失真实成本。

本次按最小改动修复可由 runtime 确定保证的契约：

- 通用 tool-result 语义接受 `started` 为非失败 domain status，并用 fanout 回执回归测试锁定；
- active→idle 交接时，只对结构化 `agent_attention_hint.v1` 做确认去重：若相同 agent/run 的 fanout result
  已被本 turn 收集，则不再启动 idle LLM turn；未收集、非 fanout 和未知通知全部保留；
- `Ctrl+T` handler 删除 background registry / spawner 依赖，始终只操作 canonical Task Board；测试使用
  真实运行中的后台任务验证它不会再打开 Background Tasks；所有后台 shell 回执统一提示 `Ctrl+B`；
- CLI child result 显式携带 `turns_completed`，spawner 在 archive / journal 前写入统一 metrics。Server delegation
  的共享结果尚未暴露 round 数时明确记 0，不使用 tool count 或 token count 猜测，避免伪造观测数据；
- Waiting 与 Stalled 分离，系统 deadline cancellation 与用户主动取消分离，避免父 agent 基于错误生命周期
  语义做升级或归因。

对多-agent review 采用以下最小 guardrail，不建设新的 verifier 框架：

- 子 agent finding 是**待验证证据**，不是 task fact。父 agent 只有在读到当前代码不变量，并得到最小复现、
  现有测试缺口或明确反例之一后，才能把 finding 建成修复任务。
- 容量策略必须先声明交付语义：已经返回 accepted 的消息不能为接收新消息而静默驱逐；bounded queue 满时
  明确拒绝新消息是 backpressure，不是资源泄漏。
- 同一 finding 被取消为 false positive 后，不因 compaction / continuation anchor 再创建等价任务；若新证据
  要求 reopen，应恢复原 task 并记录 invalidation evidence。
- `completed` 表示代码事实与验证事实都成立。若后续撤销对应 diff，Task Board 必须同步 reopen/cancel，不能
  保留与工作区相反的 completed 投影。
- 内部 notification turn 在 observability 中必须有独立 kind 和稳定 id，不能与真实用户 turn 复用编号或只靠
  空字符串区分；这属于后续 telemetry/persistence 修复，不阻塞本轮 runtime 控制闭环。

本轮没有扩展到以下能力，因为它们需要新的跨执行面契约，不能用局部开关安全完成：

- Bash 按等待预算自动 yield，以及模型显式 background 的 CLI / Server 一致实现；
- PTY capability 与 `write_stdin`；
- Server/WebSocket 的真实 stdout delta 和 process cursor parity；
- Harness run-tree identity、授权控制和审计；
- 通用 Execution、DAG、workflow DSL 或自治 supervisor。

这些仍按 P1 / P2 顺序保留。特别是 Bash 自动 yield 必须让 CLI、Server 和 task notification 共用
同一 adoption 路径后再开放，否则只在 TUI 生效的 `run_in_background` 参数会制造新的产品漂移。
