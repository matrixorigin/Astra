# Codex Client 配置实现 — 新人学习文档

## 一句话总结

我们把 Astra Gateway 中的 Codex CLI 集成从一个只能"跑起来"的 stub（桩代码），升级为与 Claude 对等的完整 client，支持流式/非流式输出、模型选择、session 恢复等能力。

---

## 项目背景：这个仓库是什么？

**Astra** 是一个 Agent 平台，核心架构如下：

```
用户 (微信/企微/...)
     │
     ▼
┌─────────────┐
│  Gateway    │  ← 接收消息，调度 CLI 后端，返回结果
└─────────────┘
     │  spawn 子进程
     ▼
┌─────────────┐
│  CLI Agent  │  ← 实际干活的 AI 编程助手
└─────────────┘
  astra / claude / codex
```

**Gateway** 是一个中间层服务，它：
1. 接收来自各平台（微信、企微等）的消息
2. 选择一个 CLI Agent 后端来处理
3. 把 CLI 的输出翻译后发回给用户

Gateway 支持多种 CLI 后端（通过 `CliProfile` 枚举），用户可以在运行时通过 `/cli` 命令切换。

---

## 改动前的状况

| Client | 能力 | 流式 | 模型选择 | Session |
|--------|------|------|---------|---------|
| Claude | 完整 | stream-json | --model | --resume |
| Codex  | 最简 | 无 | 无 | 无 |

Codex 的旧实现：
```rust
Codex {
    bin: String,          // 可执行文件路径
    approval_mode: String // "full-auto" — 已废弃的旧参数
}
```

命令构建也很粗糙：
```rust
cmd.arg(message).arg("--full-auto").arg("--json");
```

---

## 改动后的状况

| Client | 能力 | 流式 | 模型选择 | Session |
|--------|------|------|---------|---------|
| Claude | 完整 | stream-json | --model | --resume |
| **Codex** | **完整** | **--json (JSONL)** | **--model** | **exec resume** |

---

## 修改了哪些文件？为什么？

### 1. `rust/crates/astra-gateway/src/cli_bridge.rs` — 核心改动

**这个文件是什么：** Gateway 与 CLI Agent 交互的桥接层。定义了如何构建命令、如何解析输出、如何处理流式事件。

**改了什么：**

#### a) 扩展 `CliProfile::Codex` 结构体

```rust
// 新字段
Codex {
    bin: String,
    model: Option<String>,        // 模型选择 (o4-mini, gpt-5.2-codex, ...)
    sandbox: String,              // 沙箱策略 (read-only / workspace-write / danger-full-access)
    stream_json: bool,            // 是否开启 JSONL 流式输出
    extra_args: Vec<String>,      // 额外参数
    skip_git_repo_check: bool,    // 非 git 目录时跳过检查
    ephemeral: bool,              // 临时模式，不保存 session
}
```

**为什么：** 旧的 `approval_mode: "full-auto"` 是 Codex CLI 早期版本的参数，新版已废弃。新版用 `--sandbox` 控制权限，支持 `--model`、`--json` 等丰富参数。

#### b) 重写 `build_command_with_context` 中的 Codex 分支

```rust
// 新版命令构建
cmd.arg("exec").arg(message);          // 用 exec 子命令（非交互模式）
cmd.arg("--sandbox").arg(sandbox);
cmd.arg("--json");                     // JSONL 流式输出
cmd.arg("--model").arg(model);
```

**为什么：** Codex 的非交互模式必须用 `codex exec` 子命令。Session 恢复通过 `codex exec resume <session_id>` 实现。

#### c) 新增 `parse_codex_stream_json_stdout` — 最终结果解析

从 Codex 的 JSONL 输出中提取：
- `thread.started` → session_id（会话 ID）
- `turn.completed` → tokens 使用量
- `item.completed` (agent_message) → 最终文本回复
- `item.completed` (command_execution / file_change) → 工具使用统计

**为什么：** Codex 的 JSON 格式与 Claude 完全不同。Claude 用 `type: "result"` 包含最终答案，Codex 用事件流（thread → turn → item）。

#### d) 新增 `parse_codex_stream_json_line` — 实时进度解析

每一行 JSONL 实时解析为进度事件：
- `item.started` (command_execution) → "工具开始执行"
- `item.completed` (command_execution) → "工具执行完成"
- `item.started` (agent_message) → 文本 token 流

**为什么：** 让 Gateway 能实时把进度推送给用户（比如微信里看到"正在执行命令 ls -la..."），而不是等一分钟后才收到完整回复。

#### e) 更新流式 stdout 分发逻辑

```rust
let ev = match cli_name.as_str() {
    "codex" => parse_codex_stream_json_line(&line),
    _ => parse_claude_stream_json_line(&line),
};
```

**为什么：** 原来硬编码只用 Claude 的解析器。现在根据 CLI 名称选择正确的解析器。

---

### 2. `rust/crates/astra-gateway/src/runner.rs` — 运行时集成

**这个文件是什么：** Gateway 的主运行循环，处理消息调度、模型切换、用量统计。

**改了什么：** 在两处 model override 逻辑中加入 `CliProfile::Codex` 分支。

**为什么：** 用户通过 `/model o4-mini` 切换模型时，需要把选择的模型名注入到 Codex 的 profile 中。

---

### 3. `rust/crates/astra-gateway/src/gateway_context.rs` — 测试修复

**改了什么：** 更新测试中 Codex 的结构体字段，把"session 不支持"的测试改为用 Custom profile。

**为什么：** Codex 现在支持 session 了，旧测试的断言条件不成立了。用 Custom profile 代替，它确实不支持 session。

---

### 4. 配置文件

| 文件 | 说明 |
|------|------|
| `gateway.example.yaml` | 更新 codex profile 示例配置 |
| `gateway-codex-minimal.yaml` | 新增：最小化 Codex-only 部署配置 |
| `setup.sh` | 修复自动配置脚本中的旧参数 |

---

## 核心概念解释

### CliProfile（CLI 配置文件）

一个枚举，定义了不同 CLI Agent 的"身份证"：
- 用什么可执行文件
- 传什么参数
- 怎么解析输出
- 支持什么能力

### 流式 vs 非流式

| 模式 | Codex 参数 | 输出格式 | 用户体验 |
|------|-----------|---------|---------|
| 非流式 | 不加 `--json` | 纯文本 stdout | 等完了才能看到回复 |
| 流式 | `--json` | JSONL (每行一个 JSON) | 实时看到 token、工具执行进度 |

### Codex 的 JSONL 事件流示例

```jsonl
{"type":"thread.started","thread_id":"thread_abc123"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_1","type":"reasoning","text":"Let me analyze..."}}
{"type":"item.completed","item":{"id":"item_1","type":"reasoning","text":"Let me analyze the code..."}}
{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"ls -la"}}
{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"ls -la","exit_code":0,"status":"completed"}}
{"type":"item.started","item":{"id":"item_3","type":"agent_message","text":""}}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Here are the files..."}}
{"type":"turn.completed","usage":{"input_tokens":1200,"output_tokens":150}}
```

### Claude 的 stream-json 对比

```jsonl
{"type":"system","tools":["Read","Edit","Bash"]}
{"type":"assistant","message":{"content":[{"type":"text","text":"Let me check..."}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}
{"type":"result","session_id":"abc","result":"Done!","usage":{"input_tokens":500,"output_tokens":100}}
```

可以看到两者的格式完全不同，所以需要分别实现解析器。

---

## 关键设计决策

### 1. 为什么用 `codex exec` 而不是直接 `codex`？

`codex` 默认启动交互式 TUI（终端界面），需要键盘输入。`codex exec` 是非交互模式，适合被程序调用。

### 2. 为什么用 `--sandbox` 替代 `--full-auto`？

`--full-auto` 是旧版参数，已被废弃。新版 Codex CLI 把权限控制重构为 sandbox 模型，更精细：
- `read-only`：只读（安全但功能受限）
- `workspace-write`：可以修改工作目录（推荐）
- `danger-full-access`：完全不限制（危险）

### 3. 为什么 Codex 的 `--json` 本身就是流式的？

Claude 区分 `--output-format json`（最终一次性输出）和 `--output-format stream-json`（逐行流式）。

Codex 只有 `--json`，它天然就是流式的——每个事件发生时就输出一行 JSONL。这是设计哲学的差异。

### 4. 为什么 `supports_session` 改为 `true`？

因为 Codex 确实支持 session：通过 `codex exec resume <session_id>` 可以恢复之前的对话。只是实现方式不同——不是 flag 而是子命令。

---

## 如何验证这些改动？

1. **安装 Codex CLI**：`npm install -g @openai/codex`
2. **设置 API Key**：`export OPENAI_API_KEY=sk-...`
3. **用最小配置启动 Gateway**：
   ```bash
   cargo run -p astra-gateway --release -- --config rust/crates/astra-gateway/gateway-codex-minimal.yaml
   ```
4. **发送消息测试**（通过微信或 API 直接调用）

---

## 文件改动清单

```
Modified:
  rust/crates/astra-gateway/src/cli_bridge.rs       (+180, -10)  核心实现
  rust/crates/astra-gateway/src/runner.rs           (+4, -4)     运行时集成
  rust/crates/astra-gateway/src/gateway_context.rs  (+8, -3)     测试修复
  rust/crates/astra-gateway/gateway.example.yaml    (+7, -2)     配置示例
  rust/crates/astra-gateway/setup.sh               (+2, -1)     安装脚本

Added:
  rust/crates/astra-gateway/gateway-codex-minimal.yaml           最小化部署配置
```
